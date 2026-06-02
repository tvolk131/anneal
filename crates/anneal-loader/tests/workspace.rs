//! `load_workspace` enumerates every package in the tree (§4, multi-package) — the
//! whole-graph load that reverse-dependency queries like `affected` need.

use anneal_loader::load_workspace;
use anneal_rules::builtin_rules;

/// Write a `BUILD` with one genrule named `name` into `dir`.
fn build_file(dir: &std::path::Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("BUILD"),
        format!("genrule(name = \"{name}\", outs = [\"o\"], cmd = \"echo > $(OUTS)\")\n"),
    )
    .unwrap();
}

#[test]
fn enumerates_all_packages_and_skips_ignored_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    build_file(&root.join("app"), "app");
    build_file(&root.join("lib"), "lib");
    build_file(&root.join("crates/deep/nested"), "nested"); // arbitrarily deep
    // BUILD files inside ignored directories must NOT be picked up.
    build_file(&root.join("target/junk"), "junk");
    build_file(&root.join(".git/hooks"), "hook");
    build_file(&root.join("node_modules/pkg"), "vendored");

    let registry = builtin_rules();
    let graph = load_workspace(root, &registry).unwrap();

    use anneal_core::Label;
    let has = |l: &str| graph.get(&Label::parse(l).unwrap()).is_some();
    assert!(has("//app:app"));
    assert!(has("//lib:lib"));
    assert!(has("//crates/deep/nested:nested"));
    // Three real packages, nothing from target/ .git/ node_modules.
    assert_eq!(graph.len(), 3, "ignored-dir BUILD files must be skipped");
}
