//! Focus-cone coloring (DESIGN.md §4.2/§4.3): per-node `ExecMode` assignment,
//! one configuration per node per invocation, and the monotonicity theorem
//! asserted at edge resolution rather than trusted to policy.

use std::collections::HashSet;

use anneal_analysis::{AnalysisError, Analyzer};
use anneal_core::{AxisValues, Configuration, ExecMode, Label, Platform};
use anneal_exec::LocalExecutor;
use anneal_loader::load_package;
use anneal_rules::builtin_rules;

fn host_config() -> Configuration {
    Configuration::new(Platform::new("host", "host"), AxisValues::default())
}

fn workspace(build: &str, sources: &[(&str, &str)]) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("BUILD"), build).unwrap();
    for (name, contents) in sources {
        std::fs::write(pkg.join(name), contents).unwrap();
    }
    tmp
}

const BUILD: &str = r#"
filegroup(name = "data", srcs = ["a.txt"])
genrule(
    name = "combined",
    deps = ["//pkg:data"],
    outs = ["combined.txt"],
    cmd = "cat $(SRCS) > $(OUTS)",
)
"#;

#[test]
fn cone_members_build_incremental_and_upstream_builds_hermetic() {
    let tmp = workspace(BUILD, &[("a.txt", "alpha\n")]);
    let root = tmp.path();
    let registry = builtin_rules();
    let graph = load_package(root, "pkg", &registry).unwrap();
    let config = host_config();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();

    let data = Label::parse("//pkg:data").unwrap();
    let combined = Label::parse("//pkg:combined").unwrap();

    // The dependent (`combined`) is "edited"; its dependency stays upstream.
    // Monotone: the Incremental node is downstream of the Hermetic one.
    let cone: HashSet<Label> = HashSet::from([combined.clone()]);
    let analyzed = Analyzer::new(&graph, &registry, &config, root, exec.cas())
        .with_incremental_cone(cone)
        .analyze(&combined)
        .unwrap();

    assert_eq!(
        analyzed.target(&combined).unwrap().config.axes().exec_mode,
        ExecMode::Incremental,
        "cone member analyzes Incremental"
    );
    assert_eq!(
        analyzed.target(&data).unwrap().config.axes().exec_mode,
        ExecMode::Hermetic,
        "upstream of the cone analyzes Hermetic"
    );
}

#[test]
fn empty_cone_is_all_hermetic_and_no_cone_is_uniform() {
    let tmp = workspace(BUILD, &[("a.txt", "alpha\n")]);
    let root = tmp.path();
    let registry = builtin_rules();
    let graph = load_package(root, "pkg", &registry).unwrap();
    let config = host_config();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();
    let combined = Label::parse("//pkg:combined").unwrap();

    // Clean tree: empty cone — everything Hermetic.
    let analyzed = Analyzer::new(&graph, &registry, &config, root, exec.cas())
        .with_incremental_cone(HashSet::new())
        .analyze(&combined)
        .unwrap();
    for label in analyzed.order() {
        assert_eq!(
            analyzed.target(label).unwrap().config.axes().exec_mode,
            ExecMode::Hermetic
        );
    }

    // No cone: uniform coloring — the base configuration unchanged (default
    // Incremental), exactly the pre-cone behavior the --exec-mode flag forces.
    let analyzed = Analyzer::new(&graph, &registry, &config, root, exec.cas())
        .analyze(&combined)
        .unwrap();
    for label in analyzed.order() {
        assert_eq!(
            analyzed.target(label).unwrap().config.axes().exec_mode,
            ExecMode::Incremental
        );
    }
}

#[test]
fn non_monotone_cone_is_rejected_at_edge_resolution() {
    let tmp = workspace(BUILD, &[("a.txt", "alpha\n")]);
    let root = tmp.path();
    let registry = builtin_rules();
    let graph = load_package(root, "pkg", &registry).unwrap();
    let config = host_config();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();

    let data = Label::parse("//pkg:data").unwrap();
    let combined = Label::parse("//pkg:combined").unwrap();

    // A broken coloring: the *dependency* is Incremental while its dependent
    // is Hermetic — the §4.3 violation a pin flag without the monotone
    // closure would produce. The assert must catch it; a correct policy
    // (reverse-closure cone) can never construct this.
    let cone: HashSet<Label> = HashSet::from([data.clone()]);
    let err = Analyzer::new(&graph, &registry, &config, root, exec.cas())
        .with_incremental_cone(cone)
        .analyze(&combined)
        .unwrap_err();
    match err {
        AnalysisError::ConeViolation {
            hermetic,
            incremental,
        } => {
            assert_eq!(hermetic, combined);
            assert_eq!(incremental, data);
        }
        other => panic!("expected ConeViolation, got: {other}"),
    }
}
