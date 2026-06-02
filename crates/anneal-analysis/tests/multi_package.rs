//! Multi-package loading (§4): a target in one package depends on a target in
//! **another** package. `load_closure` walks the transitive package closure from the
//! requested target, so the analyzer (a single-graph consumer) sees both packages and
//! resolves the cross-package edge. This is the prerequisite for real monorepos and for
//! `affected`/`why` (§11.3).

use anneal_analysis::Analyzer;
use anneal_core::{AxisValues, Configuration, Label, Platform};
use anneal_exec::{Action, LocalExecutor};
use anneal_loader::load_closure;
use anneal_rules::builtin_rules;

fn host() -> Configuration {
    Configuration::new(Platform::new("host", "host"), AxisValues::default())
}

/// Two packages: `lib` produces `data.txt`; `app` depends on `//lib:data` and consumes
/// its output (`cat $(SRCS)`), so the edge crosses a package boundary.
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("lib")).unwrap();
    std::fs::create_dir_all(tmp.path().join("app")).unwrap();
    std::fs::write(
        tmp.path().join("lib/BUILD"),
        "genrule(name = \"data\", outs = [\"data.txt\"], cmd = \"echo libdata > $(OUTS)\")\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("app/BUILD"),
        "genrule(name = \"app\", deps = [\"//lib:data\"], outs = [\"combined.txt\"], \
         cmd = \"cat $(SRCS) > $(OUTS)\")\n",
    )
    .unwrap();
    tmp
}

#[test]
fn closure_loads_dependency_packages_transitively() {
    let tmp = fixture();
    let registry = builtin_rules();
    let app = Label::parse("//app:app").unwrap();

    let graph = load_closure(tmp.path(), &app, &registry).unwrap();

    // Only `app` was requested, yet `lib` was pulled in via the cross-package edge.
    assert!(graph.get(&app).is_some(), "requested package loaded");
    assert!(
        graph.get(&Label::parse("//lib:data").unwrap()).is_some(),
        "dependency's package loaded transitively"
    );
}

#[test]
fn cross_package_dependency_analyzes_and_builds() {
    let tmp = fixture();
    let root = tmp.path();
    let registry = builtin_rules();
    let cfg = host();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();
    let app = Label::parse("//app:app").unwrap();

    let graph = load_closure(root, &app, &registry).unwrap();
    let g = Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
        .analyze(&app)
        .unwrap();

    // The dependency (`//lib:data`) is analyzed before its dependent (`//app:app`).
    let order = g.order();
    let lib_pos = order.iter().position(|l| l == &Label::parse("//lib:data").unwrap());
    let app_pos = order.iter().position(|l| l == &app);
    assert!(lib_pos < app_pos, "dependency precedes dependent across packages");

    let actions: Vec<Action> = g.actions().cloned().collect();
    let results = exec.execute_graph(&actions).unwrap();
    assert!(results.iter().all(|r| r.success()), "all actions succeed across packages");

    // `app`'s genrule consumed `lib`'s generated file across the package boundary.
    let idx = actions.iter().position(|a| a.name() == "genrule //app:app").unwrap();
    let combined = String::from_utf8(
        exec.cas()
            .get(results[idx].outputs.get("combined.txt").unwrap())
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(combined.trim(), "libdata");
}
