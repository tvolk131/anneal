//! End-to-end tests for the `anneal` binary, driven through the process boundary
//! (`CARGO_BIN_EXE_anneal`). Fixtures use `genrule` so they need no language toolchain:
//! a plain genrule exercises `build`, and a genrule that writes the rule-agnostic
//! `ANNEAL_TEST_EXIT` marker into `results.txt` exercises the `test` summary path.

use std::path::Path;
use std::process::{Command, Output};

fn anneal(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_anneal"))
        .args(args)
        .arg("--workspace-root")
        .arg(root)
        .output()
        .expect("run anneal")
}

/// A workspace with a single package `pkg` containing the given `BUILD` contents.
fn workspace(build: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("BUILD"), build).unwrap();
    tmp
}

#[test]
fn build_runs_the_graph_and_caches() {
    let ws = workspace("genrule(name = \"gen\", outs = [\"out.txt\"], cmd = \"echo hi > $(OUTS)\")\n");

    let out = anneal(ws.path(), &["build", "//pkg:gen"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("genrule //pkg:gen"), "stdout:\n{stdout}");
    assert!(stdout.contains("build ok"), "stdout:\n{stdout}");

    // An identical re-run hits the action cache.
    let again = anneal(ws.path(), &["build", "//pkg:gen"]);
    assert!(
        String::from_utf8_lossy(&again.stdout).contains("CACHED"),
        "second build should report a cache hit"
    );
}

#[test]
fn test_summarizes_a_passing_result() {
    // A genrule that writes the test marker is, to the CLI, a passing test target.
    let ws = workspace(
        "genrule(name = \"t\", outs = [\"results.txt\"], cmd = \"printf 'ANNEAL_TEST_EXIT=0' > $(OUTS)\")\n",
    );
    let out = anneal(ws.path(), &["test", "//pkg:t"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 passed, 0 failed"),
        "stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn test_reports_a_failing_result_with_nonzero_exit() {
    // The action succeeds (printf exits 0) but records a failing test exit — the
    // always-exit-0 test idiom. The CLI must surface that as a failure.
    let ws = workspace(
        "genrule(name = \"t\", outs = [\"results.txt\"], cmd = \"printf 'ANNEAL_TEST_EXIT=1' > $(OUTS)\")\n",
    );
    let out = anneal(ws.path(), &["test", "//pkg:t"]);
    assert_eq!(out.status.code(), Some(1), "a failing test exits 1");
    assert!(String::from_utf8_lossy(&out.stdout).contains("0 passed, 1 failed"));
}

#[test]
fn unknown_target_and_bad_flags_exit_2() {
    let ws = workspace("genrule(name = \"gen\", outs = [\"o\"], cmd = \"echo x > $(OUTS)\")\n");

    let unknown = anneal(ws.path(), &["build", "//pkg:nope"]);
    assert_eq!(unknown.status.code(), Some(2), "unknown target is a usage error");
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("error:"));

    let bad_flag = anneal(ws.path(), &["build", "//pkg:gen", "--opt-level", "bogus"]);
    assert_eq!(bad_flag.status.code(), Some(2), "an invalid axis value is a usage error");
}

/// A `base → lib → app` chain across three packages, with a tracked file in `base`.
fn chain_workspace() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let write = |pkg: &str, build: &str| {
        let dir = tmp.path().join(pkg);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("BUILD"), build).unwrap();
    };
    write("base", "genrule(name = \"base\", outs = [\"b\"], cmd = \"echo > $(OUTS)\")\n");
    write("lib", "genrule(name = \"lib\", deps = [\"//base:base\"], outs = [\"l\"], cmd = \"echo > $(OUTS)\")\n");
    write("app", "genrule(name = \"app\", deps = [\"//lib:lib\"], outs = [\"a\"], cmd = \"echo > $(OUTS)\")\n");
    std::fs::write(tmp.path().join("base/data.txt"), "orig").unwrap();
    tmp
}

#[test]
fn why_shows_a_path_and_requires_a_query() {
    let ws = chain_workspace();
    let out = anneal(ws.path(), &["why", "//app:app", "//base:base"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("//app:app → //lib:lib → //base:base"),
        "stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    // No path between unrelated targets is reported, not an error.
    let none = anneal(ws.path(), &["why", "//base:base", "//app:app"]);
    assert!(String::from_utf8_lossy(&none.stdout).contains("no path"));
    // Neither <to> nor --since is a usage error.
    let bad = anneal(ws.path(), &["why", "//app:app"]);
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn affected_and_why_since_track_a_git_change() {
    let ws = chain_workspace();
    let root = ws.path();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(root)
            .status()
            .expect("git available")
            .success();
        assert!(ok, "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "base"]);
    // Modify a tracked file in `base`.
    std::fs::write(root.join("base/data.txt"), "changed").unwrap();

    // affected --since lists base and everything that transitively depends on it.
    let aff = anneal(root, &["affected", "--since", "HEAD"]);
    assert!(aff.status.success(), "stderr: {}", String::from_utf8_lossy(&aff.stderr));
    let aff_out = String::from_utf8_lossy(&aff.stdout);
    for label in ["//app:app", "//base:base", "//lib:lib"] {
        assert!(aff_out.contains(label), "affected should include {label}; got:\n{aff_out}");
    }

    // why --since explains app's affectedness with the path to the change.
    let why = anneal(root, &["why", "//app:app", "--since", "HEAD"]);
    assert!(
        String::from_utf8_lossy(&why.stdout).contains("//app:app → //lib:lib → //base:base"),
        "stdout:\n{}",
        String::from_utf8_lossy(&why.stdout)
    );
}
