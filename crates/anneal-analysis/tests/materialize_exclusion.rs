//! Materialize exclusion, end to end: a tree copy written by `anneal
//! materialize` (a generated output parked in the working tree for native
//! tools) must be invisible to source discovery. Without the exclusion the
//! copy is recorded as a source and collides with the producing action's
//! declared output — `validate_generated_paths` fails the whole analysis.
//! With it, analysis succeeds *and* the consuming action's cache key is
//! byte-identical to the no-copy baseline, so materializing never perturbs
//! caching or snapshot keys.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anneal_analysis::{ActionGraph, AnalysisError, Analyzer};
use anneal_core::{AxisValues, Configuration, Label, Platform};
use anneal_exec::{action_digest, Action, LocalExecutor};
use anneal_loader::load_package;
use anneal_rules::{builtin_rules, ArtifactSource};

/// A dependency-free Cargo workspace under `<tmp>/ws` whose `cargo_workspace`
/// target routes a genrule-generated `config.json` in via `data` — the exact
/// shape `anneal materialize` parks in the tree.
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(ws.join("mylib/src")).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        "[workspace]\nmembers = [\"mylib\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("mylib/Cargo.toml"),
        "[package]\nname = \"mylib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("mylib/src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("BUILD"),
        "genrule(name = \"gen\", outs = [\"config.json\"], cmd = \"printf '{}' > $(OUTS)\")\n\
         cargo_workspace(name = \"ws\", data = [\"//ws:gen\"])\n",
    )
    .unwrap();

    let status = std::process::Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&ws)
        .status()
        .expect("cargo must be available to set up the fixture");
    assert!(status.success(), "cargo generate-lockfile failed");
    tmp
}

fn analyze(
    root: &std::path::Path,
    exec: &LocalExecutor,
    materialized: BTreeSet<PathBuf>,
) -> Result<ActionGraph, AnalysisError> {
    let registry = builtin_rules();
    let graph = load_package(root, "ws", &registry).unwrap();
    let cfg = Configuration::new(Platform::new("host", "host"), AxisValues::default());
    Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
        .with_materialized_paths(materialized)
        .analyze(&Label::parse("//ws:ws").unwrap())
}

fn build_action(graph: &ActionGraph) -> Action {
    graph
        .actions()
        .find(|a| a.name().starts_with("cargo_workspace build"))
        .expect("a build action")
        .clone()
}

#[test]
fn tree_copy_fails_analysis_without_exclusion_and_is_invisible_with_it() {
    let tmp = fixture();
    let root = tmp.path();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();

    // Baseline: no tree copy. Analysis succeeds; remember the build action's key.
    let baseline = analyze(root, &exec, BTreeSet::new()).unwrap();
    let baseline_digest = action_digest(&build_action(&baseline));

    // Park the generated file in the tree, as `anneal materialize` would.
    std::fs::write(root.join("ws/config.json"), b"{}").unwrap();

    // Without the exclusion, the copy is walked as a source and shadows the
    // genrule's declared output: analysis hard-fails.
    let err = analyze(root, &exec, BTreeSet::new()).unwrap_err();
    assert!(
        matches!(
            &err,
            AnalysisError::GeneratedOutputShadowsSource { path, .. }
                if path == &PathBuf::from("ws/config.json")
        ),
        "expected GeneratedOutputShadowsSource, got: {err}"
    );

    // With the manifest-driven exclusion, the copy is invisible: analysis
    // succeeds and the build action's cache key is byte-identical to the
    // baseline — materializing perturbs neither caching nor snapshot keys.
    let excluded: BTreeSet<PathBuf> = [PathBuf::from("ws/config.json")].into();
    let graph = analyze(root, &exec, excluded).unwrap();
    assert_eq!(action_digest(&build_action(&graph)), baseline_digest);
}

/// The consumer's routed data is what `anneal materialize` parks: the cargo
/// target (not the genrule) declares it, and the analyzer re-homes the rule's
/// package-relative destination to a workspace-relative one.
#[test]
fn consumer_exposes_routed_data_at_workspace_relative_dest() {
    let tmp = fixture();
    let root = tmp.path();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();
    let graph = analyze(root, &exec, BTreeSet::new()).unwrap();

    let routed = graph
        .routed_data(&Label::parse("//ws:ws").unwrap())
        .expect("//ws:ws analyzed");
    assert_eq!(routed.len(), 1);
    assert_eq!(routed[0].path, PathBuf::from("ws/config.json"));
    assert!(
        matches!(
            &routed[0].source,
            ArtifactSource::Output { action, name }
                if action == "genrule //ws:gen" && name == "config.json"
        ),
        "routed data should reference the producing genrule's output, got {:?}",
        routed[0].source
    );

    // The producer routes nothing — materialize is consumer-oriented.
    let gen_routed = graph
        .routed_data(&Label::parse("//ws:gen").unwrap())
        .expect("//ws:gen analyzed");
    assert!(gen_routed.is_empty());
}
