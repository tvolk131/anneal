//! Analysis end-to-end: load a multi-target `BUILD`, analyze it into an action
//! graph with provider threading and memoization, and execute the graph.

use anneal_analysis::{AnalysisError, Analyzer};
use anneal_core::{AxisValues, Configuration, Label, Platform};
use anneal_exec::{Executor, LocalExecutor};
use anneal_loader::load_package;
use anneal_rules::builtin_rules;

fn host_config() -> Configuration {
    Configuration::new(Platform::new("host", "host"), AxisValues::default())
}

/// A workspace on disk with one package; returns (tempdir, root path).
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

#[test]
fn filegroup_to_genrule_threads_providers_and_executes() {
    let tmp = workspace(
        r#"
filegroup(name = "data", srcs = ["a.txt", "b.txt"])
genrule(
    name = "combined",
    deps = ["//pkg:data"],
    outs = ["combined.txt"],
    cmd = "cat $(SRCS) > $(OUTS)",
)
"#,
        &[("a.txt", "alpha\n"), ("b.txt", "beta\n")],
    );
    let root = tmp.path();

    let registry = builtin_rules();
    let graph = load_package(root, "pkg", &registry).unwrap();
    let config = host_config();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();

    let analyzer = Analyzer::new(&graph, &registry, &config, root, exec.cas());
    let combined = Label::parse("//pkg:combined").unwrap();
    let action_graph = analyzer.analyze(&combined).unwrap();

    // Dependency order: filegroup precedes genrule; only genrule contributes an action.
    assert_eq!(
        action_graph.order(),
        &[Label::parse("//pkg:data").unwrap(), combined.clone()]
    );
    assert_eq!(action_graph.action_count(), 1);

    // Execute every action in dependency order.
    let mut last = None;
    for action in action_graph.actions() {
        last = Some(exec.execute(action).unwrap());
    }
    let result = last.unwrap();
    assert!(result.success());
    // The genrule concatenated the filegroup's two provided files.
    let out = exec
        .cas()
        .get(result.outputs.get("combined.txt").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "alpha\nbeta\n");
}

#[test]
fn genrule_consumes_another_genrules_output() {
    // The chained case the action-graph model unlocks: `second` depends on `first`'s
    // *produced* output, whose digest is unknown until `first` runs.
    let tmp = workspace(
        r#"
genrule(name = "first", srcs = ["base.txt"], outs = ["first.txt"], cmd = "cat $(SRCS) > $(OUTS)")
genrule(
    name = "second",
    deps = ["//pkg:first"],
    outs = ["second.txt"],
    cmd = "cat $(SRCS) > $(OUTS); echo extra >> $(OUTS)",
)
"#,
        &[("base.txt", "hello\n")],
    );
    let root = tmp.path();
    let registry = builtin_rules();
    let graph = load_package(root, "pkg", &registry).unwrap();
    let config = host_config();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();

    let analyzer = Analyzer::new(&graph, &registry, &config, root, exec.cas());
    let g = analyzer
        .analyze(&Label::parse("//pkg:second").unwrap())
        .unwrap();

    // Both genrules contribute an action; `first` precedes `second`.
    assert_eq!(
        g.order(),
        &[
            Label::parse("//pkg:first").unwrap(),
            Label::parse("//pkg:second").unwrap()
        ]
    );
    assert_eq!(g.action_count(), 2);

    // execute_graph threads `first`'s output into `second`'s input.
    let actions: Vec<_> = g.actions().cloned().collect();
    let results = exec.execute_graph(&actions).unwrap();
    let second = results.last().unwrap();
    assert!(second.success());
    let out = exec
        .cas()
        .get(second.outputs.get("second.txt").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "hello\nextra\n");
}

#[test]
fn alias_forwards_providers_through_analysis() {
    let tmp = workspace(
        r#"
filegroup(name = "data", srcs = ["x.txt"])
alias(name = "data_alias", actual = "//pkg:data")
"#,
        &[("x.txt", "x")],
    );
    let root = tmp.path();
    let registry = builtin_rules();
    let graph = load_package(root, "pkg", &registry).unwrap();
    let config = host_config();
    let cas = anneal_cas::Cas::open(root.join("cas")).unwrap();

    let analyzer = Analyzer::new(&graph, &registry, &config, root, &cas);
    let g = analyzer
        .analyze(&Label::parse("//pkg:data_alias").unwrap())
        .unwrap();

    let alias_providers = g
        .providers(&Label::parse("//pkg:data_alias").unwrap())
        .unwrap();
    let data_providers = g.providers(&Label::parse("//pkg:data").unwrap()).unwrap();
    assert_eq!(
        alias_providers, data_providers,
        "alias forwards data's providers"
    );
    assert_eq!(action_count_for_files(alias_providers), 1);
}

fn action_count_for_files(p: &anneal_rules::ProviderSet) -> usize {
    p.files.as_ref().map(|fs| fs.files.len()).unwrap_or(0)
}

#[test]
fn diamond_dependency_is_analyzed_once() {
    // Two genrules both depend on the same filegroup; analyzing a top alias that
    // points at one of them still visits the filegroup exactly once.
    let tmp = workspace(
        r#"
filegroup(name = "data", srcs = ["a.txt"])
genrule(name = "left", deps = ["//pkg:data"], outs = ["l.txt"], cmd = "cat $(SRCS) > $(OUTS)")
genrule(name = "right", deps = ["//pkg:data"], outs = ["r.txt"], cmd = "cat $(SRCS) > $(OUTS)")
"#,
        &[("a.txt", "a")],
    );
    let root = tmp.path();
    let registry = builtin_rules();
    let graph = load_package(root, "pkg", &registry).unwrap();
    let config = host_config();
    let cas = anneal_cas::Cas::open(root.join("cas")).unwrap();
    let analyzer = Analyzer::new(&graph, &registry, &config, root, &cas);

    // Analyze "left"; the filegroup appears exactly once in the order.
    let g = analyzer
        .analyze(&Label::parse("//pkg:left").unwrap())
        .unwrap();
    let data = Label::parse("//pkg:data").unwrap();
    assert_eq!(g.order().iter().filter(|l| **l == data).count(), 1);
}

#[test]
fn missing_dependency_is_reported() {
    let tmp = workspace(
        r#"genrule(name = "g", deps = ["//pkg:nope"], outs = ["o"], cmd = "true")"#,
        &[],
    );
    let root = tmp.path();
    let registry = builtin_rules();
    let graph = load_package(root, "pkg", &registry).unwrap();
    let config = host_config();
    let cas = anneal_cas::Cas::open(root.join("cas")).unwrap();
    let analyzer = Analyzer::new(&graph, &registry, &config, root, &cas);

    let err = analyzer
        .analyze(&Label::parse("//pkg:g").unwrap())
        .unwrap_err();
    assert!(matches!(err, AnalysisError::MissingTarget(l) if l.target() == "nope"));
}

#[test]
fn dependency_cycle_is_detected() {
    // Two aliases pointing at each other form a cycle.
    let tmp = workspace(
        r#"
alias(name = "a", actual = "//pkg:b")
alias(name = "b", actual = "//pkg:a")
"#,
        &[],
    );
    let root = tmp.path();
    let registry = builtin_rules();
    let graph = load_package(root, "pkg", &registry).unwrap();
    let config = host_config();
    let cas = anneal_cas::Cas::open(root.join("cas")).unwrap();
    let analyzer = Analyzer::new(&graph, &registry, &config, root, &cas);

    let err = analyzer
        .analyze(&Label::parse("//pkg:a").unwrap())
        .unwrap_err();
    assert!(
        matches!(err, AnalysisError::Cycle(_)),
        "expected a cycle, got {err}"
    );
}
