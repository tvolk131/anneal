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
