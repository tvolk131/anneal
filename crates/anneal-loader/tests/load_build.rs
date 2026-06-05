//! Loader tests, plus the Phase 2 exit criterion: a genrule written in a real
//! `BUILD` file loads, analyzes, and executes through the kernel, producing a cached
//! output.

use anneal_core::Label;
use anneal_loader::{load_package, load_package_str, LoadError};
use anneal_rules::builtin_rules;

fn load(src: &str) -> Result<anneal_loader::TargetGraph, LoadError> {
    load_package_str("pkg", "pkg/BUILD", src, &builtin_rules())
}

#[test]
fn every_registered_rule_kind_has_a_loader_global() {
    // Guards against loader/registry drift: each kind in the registry must be a
    // callable Starlark global. A missing global yields "Variable ... not found";
    // a present one yields at worst a schema error — never a "not found".
    let registry = builtin_rules();
    for kind in registry.kinds() {
        let src = format!(r#"{kind}(name = "x")"#);
        if let Err(err) = load_package_str("pkg", "pkg/BUILD", &src, &registry) {
            assert!(
                !err.message().contains("not found"),
                "rule `{kind}` is in the registry but has no loader global: {}",
                err.message()
            );
        }
    }
}

#[test]
fn loads_all_three_rule_kinds() {
    let graph = load(
        r#"
filegroup(name = "data", srcs = ["a.txt", "b.txt"])
genrule(
    name = "combined",
    srcs = ["a.txt", "b.txt"],
    outs = ["combined.txt"],
    cmd = "cat $(SRCS) > $(OUTS)",
)
alias(name = "c", actual = "//pkg:combined")
"#,
    )
    .unwrap();

    assert_eq!(graph.len(), 3);

    let genrule = graph.get(&Label::parse("//pkg:combined").unwrap()).unwrap();
    assert_eq!(genrule.kind, "genrule");
    assert_eq!(
        genrule.attrs.string("cmd").unwrap(),
        "cat $(SRCS) > $(OUTS)"
    );
    assert_eq!(
        genrule.attrs.string_list("srcs").unwrap(),
        &["a.txt".to_owned(), "b.txt".to_owned()]
    );

    // alias's `actual` label becomes a dependency edge.
    let alias = graph.get(&Label::parse("//pkg:c").unwrap()).unwrap();
    assert_eq!(alias.deps, vec![Label::parse("//pkg:combined").unwrap()]);
}

#[test]
fn missing_required_attribute_is_located() {
    let err = load(r#"genrule(name = "x", cmd = "true")"#).unwrap_err();
    assert!(err.message().contains("outs"), "message: {}", err.message());
    // Schema errors carry the call-site location.
    let loc = err.location().expect("located");
    assert!(loc.contains("pkg/BUILD"), "location: {loc}");
}

#[test]
fn unknown_attribute_is_rejected() {
    let err = load(r#"filegroup(name = "x", srcs = ["a"], bogus = "y")"#).unwrap_err();
    assert!(
        err.message().contains("bogus"),
        "message: {}",
        err.message()
    );
}

#[test]
fn wrong_type_is_rejected() {
    // `cmd` must be a string, not a list.
    let err =
        load(r#"genrule(name = "x", outs = ["o"], cmd = ["not", "a", "string"])"#).unwrap_err();
    assert!(err.message().contains("cmd"), "message: {}", err.message());
    assert!(
        err.message().contains("string"),
        "message: {}",
        err.message()
    );
}

#[test]
fn duplicate_target_name_is_rejected() {
    let err = load(
        r#"
filegroup(name = "dup", srcs = ["a"])
filegroup(name = "dup", srcs = ["b"])
"#,
    )
    .unwrap_err();
    assert!(
        err.message().contains("duplicate"),
        "message: {}",
        err.message()
    );
}

#[test]
fn syntax_error_has_a_location() {
    let err = load("genrule(name = ").unwrap_err();
    assert!(err.location().is_some(), "parse errors should be located");
}

#[test]
fn invalid_label_in_attribute_is_rejected() {
    // `actual` is a label-typed attribute; a non-label string must fail.
    let err = load(r#"alias(name = "a", actual = "not a label")"#).unwrap_err();
    assert!(
        err.message().contains("actual"),
        "message: {}",
        err.message()
    );
}

/// Phase 2 exit criterion, end to end.
#[test]
fn genrule_from_build_file_loads_analyzes_and_executes() {
    use anneal_core::{AxisValues, Configuration, Platform};
    use anneal_exec::{Executor, LocalExecutor};
    use anneal_rules::RuleContext;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let pkg_dir = root.join("greeter");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(pkg_dir.join("hello.txt"), "hello\n").unwrap();
    std::fs::write(pkg_dir.join("world.txt"), "world\n").unwrap();
    std::fs::write(
        pkg_dir.join("BUILD"),
        r#"
genrule(
    name = "greeting",
    srcs = ["hello.txt", "world.txt"],
    outs = ["greeting.txt"],
    cmd = "cat $(SRCS) > $(OUTS)",
)
"#,
    )
    .unwrap();

    // 1. Load the real BUILD file from disk.
    let registry = builtin_rules();
    let graph = load_package(root, "greeter", &registry).unwrap();
    let decl = graph
        .get(&Label::parse("//greeter:greeting").unwrap())
        .unwrap();

    // 2. Analyze the loaded target through its rule.
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();
    let config = Configuration::new(Platform::new("host", "host"), AxisValues::default());
    let ctx = RuleContext::new(
        decl.label.clone(),
        &decl.attrs,
        &config,
        &pkg_dir,
        exec.cas(),
        &[],
    );
    let rule = registry.get(&decl.kind).unwrap();
    let analysis = rule.analyze(&ctx).unwrap();
    assert_eq!(analysis.actions.len(), 1);

    // 3. Execute through the kernel: real run, correct output, then a cache hit.
    let action = &analysis.actions[0];
    let first = exec.execute(action).unwrap();
    assert!(first.success() && !first.cache_hit);
    let out = exec
        .cas()
        .get(first.outputs.get("greeting.txt").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "hello\nworld\n");

    let second = exec.execute(action).unwrap();
    assert!(second.cache_hit);
    assert_eq!(second.outputs, first.outputs);
}
