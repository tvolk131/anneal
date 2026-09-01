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
    // Caching is the `deterministic = True` opt-in (an arbitrary command is
    // NonCacheable by default — the engine cannot assume its purity).
    let ws = workspace(
        "genrule(name = \"gen\", outs = [\"out.txt\"], cmd = \"echo hi > $(OUTS)\", deterministic = True)\n",
    );

    let out = anneal(ws.path(), &["build", "//pkg:gen"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("genrule //pkg:gen"), "stdout:\n{stdout}");
    assert!(stdout.contains("build ok"), "stdout:\n{stdout}");

    // The summary reports the demand split (here 1/1 — a single-action
    // closure demands everything it analyzed).
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("demanded"),
        "the build summary reports demand: {}",
        String::from_utf8_lossy(&out.stdout)
    );

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
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    assert_eq!(
        unknown.status.code(),
        Some(2),
        "unknown target is a usage error"
    );
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("error:"));

    let bad_flag = anneal(ws.path(), &["build", "//pkg:gen", "--opt-level", "bogus"]);
    assert_eq!(
        bad_flag.status.code(),
        Some(2),
        "an invalid axis value is a usage error"
    );
}

/// A `base → lib → app` chain across three packages, with a tracked file in `base`.
fn chain_workspace() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let write = |pkg: &str, build: &str| {
        let dir = tmp.path().join(pkg);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("BUILD"), build).unwrap();
    };
    write(
        "base",
        "genrule(name = \"base\", outs = [\"b\"], cmd = \"echo > $(OUTS)\")\n",
    );
    write("lib", "genrule(name = \"lib\", deps = [\"//base:base\"], outs = [\"l\"], cmd = \"echo > $(OUTS)\")\n");
    write("app", "genrule(name = \"app\", deps = [\"//lib:lib\"], outs = [\"a\"], cmd = \"echo > $(OUTS)\")\n");
    std::fs::write(tmp.path().join("base/data.txt"), "orig").unwrap();
    tmp
}

#[test]
fn why_shows_a_path_and_requires_a_query() {
    let ws = chain_workspace();
    let out = anneal(ws.path(), &["why", "//app:app", "//base:base"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "init.defaultBranch=main",
            ])
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
    assert!(
        aff.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&aff.stderr)
    );
    let aff_out = String::from_utf8_lossy(&aff.stdout);
    for label in ["//app:app", "//base:base", "//lib:lib"] {
        assert!(
            aff_out.contains(label),
            "affected should include {label}; got:\n{aff_out}"
        );
    }

    // why --since explains app's affectedness with the path to the change.
    let why = anneal(root, &["why", "//app:app", "--since", "HEAD"]);
    assert!(
        String::from_utf8_lossy(&why.stdout).contains("//app:app → //lib:lib → //base:base"),
        "stdout:\n{}",
        String::from_utf8_lossy(&why.stdout)
    );
}

// --- The CI oracle: `affected --base` / `--format json` / the untracked fix -------------

use std::process::Command as GitCommand;

fn git(root: &Path, args: &[&str]) {
    let out = GitCommand::new("git")
        .args([
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A git repo whose `main` has one commit; HEAD sits on a `feature` branch
/// with one committed change and one uncommitted untracked file.
fn oracle_workspace(build: &str) -> tempfile::TempDir {
    let tmp = workspace(build);
    let pkg = tmp.path().join("pkg");
    std::fs::write(pkg.join("src.txt"), "v1\n").unwrap();
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    git(tmp.path(), &["add", "."]);
    git(tmp.path(), &["commit", "-q", "-m", "base"]);
    git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    std::fs::write(pkg.join("src.txt"), "v2\n").unwrap();
    std::fs::write(pkg.join("brand-new.txt"), "n\n").unwrap(); // committed change + …
    git(tmp.path(), &["add", "."]);
    git(tmp.path(), &["commit", "-q", "-m", "feature work"]);
    std::fs::write(pkg.join("uncommitted.txt"), "u\n").unwrap(); // …an untracked one
                                                                 // The target branch moves forward: a literal `--since main` diff would
                                                                 // wrongly include upstream.txt in this branch's change set.
    git(tmp.path(), &["stash", "-q", "--include-untracked"]);
    git(tmp.path(), &["checkout", "-q", "main"]);
    std::fs::write(pkg.join("upstream.txt"), "u\n").unwrap();
    git(tmp.path(), &["add", "."]);
    git(tmp.path(), &["commit", "-q", "-m", "upstream moves"]);
    git(tmp.path(), &["checkout", "-q", "feature"]);
    git(tmp.path(), &["stash", "pop", "-q"]);
    tmp
}

const SIMPLE_GENRULE: &str =
    "genrule(name = \"gen\", srcs = [\"src.txt\"], outs = [\"out.txt\"], cmd = \"cp $(SRCS) $(OUTS)\", deterministic = True)\n";

#[test]
fn affected_base_scopes_to_this_branch_and_includes_untracked() {
    let ws = oracle_workspace(SIMPLE_GENRULE);
    let out = anneal(ws.path(), &["affected", "--base", "main"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("//pkg:gen"),
        "the changed package's target is affected:\n{stdout}"
    );
    // Exit code 0 with changes; and "no changes" is also success (the answer
    // is the output, not the code).
}

#[test]
fn affected_since_also_includes_untracked_files() {
    // The TODO item named this form specifically: `--since` must union
    // untracked-but-unignored files exactly like `--base` does.
    let ws = oracle_workspace(SIMPLE_GENRULE);
    let out = anneal(
        ws.path(),
        &["affected", "--since", "main", "--format", "json"],
    );
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    let files = v["changed_files"].as_array().unwrap();
    assert!(
        files.iter().any(|f| f == "pkg/uncommitted.txt"),
        "--since must include untracked files:\n{files:?}"
    );
}

#[test]
fn affected_json_is_machine_readable() {
    let ws = oracle_workspace(SIMPLE_GENRULE);
    let out = anneal(
        ws.path(),
        &["affected", "--base", "main", "--format", "json"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("one JSON object");
    assert_eq!(v["workspace_wide"], false);
    assert_eq!(v["targets"], serde_json::json!(["//pkg:gen"]));
    let files = v["changed_files"].as_array().unwrap();
    assert!(
        files.iter().any(|f| f == "pkg/uncommitted.txt"),
        "untracked files are changes:\n{files:?}"
    );
    assert!(files.iter().any(|f| f == "pkg/src.txt"));
    assert!(
        !files.iter().any(|f| f == "pkg/upstream.txt"),
        "the target branch's own movement is not this branch's change set"
    );
    assert!(v["base"].as_str().unwrap().contains("merge-base"));
}

#[test]
fn affected_with_no_changes_exits_zero() {
    let tmp = workspace(SIMPLE_GENRULE);
    std::fs::write(tmp.path().join("pkg/src.txt"), "v1\n").unwrap();
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    git(tmp.path(), &["add", "."]);
    git(tmp.path(), &["commit", "-q", "-m", "base"]);
    let out = anneal(tmp.path(), &["affected", "--base", "main"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("no changes"));

    // The format is part of the contract even for the empty answer: JSON in,
    // JSON out — a consumer must never hit a parse error on success.
    let out = anneal(
        tmp.path(),
        &["affected", "--base", "main", "--format", "json"],
    );
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .expect("no-changes is still one JSON object");
    assert_eq!(v["changed_files"], serde_json::json!([]));
    assert_eq!(v["targets"], serde_json::json!([]));
    assert_eq!(v["workspace_wide"], false);
}

#[test]
fn affected_requires_a_ref() {
    let tmp = workspace(SIMPLE_GENRULE);
    std::fs::write(tmp.path().join("pkg/src.txt"), "v1\n").unwrap();
    let out = anneal(tmp.path(), &["affected"]);
    assert!(!out.status.success(), "no ref is a usage error, not a pass");
}

/// `test` through an alias must find the aliased target's tests: aliases
/// forward providers but emit no actions, so the CLI resolves the chain
/// before deriving demand terminals.
#[test]
fn test_through_an_alias_runs_the_aliased_targets_tests() {
    let ws = workspace(
        "genrule(name = \"t\", outs = [\"results.txt\"], cmd = \"printf 'ANNEAL_TEST_EXIT=0' > $(OUTS)\", deterministic = True)\nalias(name = \"ta\", actual = \"//pkg:t\")\n",
    );
    let out = anneal(ws.path(), &["test", "//pkg:ta"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("tests: 1 passed"),
        "the alias must resolve to the real target's tests:\n{stdout}"
    );
    assert!(!stdout.contains("no test targets found"));

    // And build through the alias still works (providers forward).
    let out = anneal(ws.path(), &["build", "//pkg:ta"]);
    assert!(out.status.success());
}

// --- Execute mode: `test --base` -------------------------------------------------

/// A repo with two independent test targets and a shared build dep.
fn exec_ws(a_test_exit: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    for (pkg, build) in [
        (
            "lib",
            "genrule(name = \"lib\", srcs = [\"lib.txt\"], outs = [\"lib.out\"], cmd = \"cp $(SRCS) $(OUTS)\", deterministic = True)\n",
        ),
        (
            "pkg-a",
            &format!(
                "genrule(name = \"t\", srcs = [\"a.txt\"], deps = [\"//lib:lib\"], outs = [\"results.txt\"], cmd = \"printf 'ANNEAL_TEST_EXIT={a_test_exit}' > $(OUTS)\", deterministic = True)\n"
            ),
        ),
        (
            "pkg-b",
            "genrule(name = \"t\", srcs = [\"b.txt\"], deps = [\"//lib:lib\"], outs = [\"results.txt\"], cmd = \"printf 'ANNEAL_TEST_EXIT=0' > $(OUTS)\", deterministic = True)\n",
        ),
    ] {
        let dir = tmp.path().join(pkg);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("BUILD"), build).unwrap();
    }
    std::fs::write(tmp.path().join("lib/lib.txt"), "l\n").unwrap();
    std::fs::write(tmp.path().join("pkg-a/a.txt"), "a\n").unwrap();
    std::fs::write(tmp.path().join("pkg-b/b.txt"), "b\n").unwrap();
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    git(tmp.path(), &["add", "."]);
    git(tmp.path(), &["commit", "-q", "-m", "base"]);
    git(tmp.path(), &["checkout", "-q", "-b", "feature"]);
    tmp
}

#[test]
fn test_base_runs_only_the_affected_cone() {
    let ws = exec_ws("0");
    std::fs::write(ws.path().join("pkg-a/a.txt"), "a2\n").unwrap();
    git(ws.path(), &["add", "."]);
    git(ws.path(), &["commit", "-q", "-m", "edit pkg-a"]);

    let out = anneal(ws.path(), &["test", "--base", "main"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("//pkg-a:t"), "pkg-a's test runs:\n{stdout}");
    assert!(
        !stdout.contains("//pkg-b:t"),
        "pkg-b is unaffected — its test must not run:\n{stdout}"
    );
    // The shared dep builds exactly once even though both closures contain it
    // — wait, only pkg-a's closure is demanded here; the dedup test is below.
    assert!(stdout.contains("tests: 1 passed, 0 failed"), "{stdout}");
}

#[test]
fn test_base_dedupes_shared_dependencies() {
    // Both packages depend on //lib:lib; editing the lib makes both affected,
    // and lib's build action must execute once across the two demand unions.
    let ws = exec_ws("0");
    std::fs::write(ws.path().join("lib/lib.txt"), "l2\n").unwrap();
    git(ws.path(), &["add", "."]);
    git(ws.path(), &["commit", "-q", "-m", "edit lib"]);

    let out = anneal(ws.path(), &["test", "--base", "main"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lib_runs = stdout
        .lines()
        .filter(|l| l.contains("genrule //lib:lib"))
        .count();
    assert_eq!(
        lib_runs, 1,
        "the shared dep executes exactly once:\n{stdout}"
    );
    assert!(stdout.contains("tests: 2 passed, 0 failed"), "{stdout}");
}

#[test]
fn test_base_fails_on_a_failing_affected_test() {
    let ws = exec_ws("1"); // pkg-a's test records a failure
    std::fs::write(ws.path().join("pkg-a/a.txt"), "a2\n").unwrap();
    git(ws.path(), &["add", "."]);
    git(ws.path(), &["commit", "-q", "-m", "edit pkg-a"]);

    let out = anneal(ws.path(), &["test", "--base", "main"]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("tests: 0 passed, 1 failed"), "{stdout}");
}

#[test]
fn test_base_with_no_changes_is_green_and_instant() {
    let ws = exec_ws("0");
    let out = anneal(ws.path(), &["test", "--base", "main"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("no changes"));
}

#[test]
fn test_base_counts_untracked_files() {
    let ws = exec_ws("0");
    std::fs::write(ws.path().join("pkg-a/new.txt"), "n\n").unwrap(); // never committed
    let out = anneal(ws.path(), &["test", "--base", "main"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("//pkg-a:t"),
        "untracked changes count:\n{stdout}"
    );
}

#[test]
fn test_base_on_a_bad_ref_is_could_not_determine() {
    let ws = exec_ws("0");
    let out = anneal(ws.path(), &["test", "--base", "no-such-ref"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn test_base_second_run_serves_cache_hits() {
    let ws = exec_ws("0");
    std::fs::write(ws.path().join("pkg-a/a.txt"), "a2\n").unwrap();
    git(ws.path(), &["add", "."]);
    git(ws.path(), &["commit", "-q", "-m", "edit pkg-a"]);
    assert!(anneal(ws.path(), &["test", "--base", "main"])
        .status
        .success());
    let again = anneal(ws.path(), &["test", "--base", "main"]);
    assert!(again.status.success());
    assert!(String::from_utf8_lossy(&again.stdout).contains("cached"));
}

#[test]
fn test_requires_a_target_or_base() {
    let tmp = workspace("filegroup(name = \"f\", srcs = [\"x\"])\n");
    std::fs::write(tmp.path().join("pkg/x"), "x\n").unwrap();
    let out = anneal(tmp.path(), &["test"]);
    assert!(!out.status.success());
}

#[test]
fn test_base_on_an_unowned_change_goes_workspace_wide() {
    // A change outside any package can't be scoped — everything is affected,
    // loudly: both packages' tests run.
    let ws = exec_ws("0");
    std::fs::write(ws.path().join("README.md"), "unowned\n").unwrap();
    git(ws.path(), &["add", "."]);
    git(ws.path(), &["commit", "-q", "-m", "readme"]);

    let out = anneal(ws.path(), &["test", "--base", "main"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("//pkg-a:t"), "{stdout}");
    assert!(stdout.contains("//pkg-b:t"), "{stdout}");
    assert!(stdout.contains("tests: 2 passed, 0 failed"), "{stdout}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("whole workspace"),
        "the conservative fallback must be announced"
    );
}

#[test]
fn test_target_and_base_are_mutually_exclusive() {
    let ws = exec_ws("0");
    let out = anneal(ws.path(), &["test", "//pkg-a:t", "--base", "main"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "clap rejects the pair with a usage error, not a panic"
    );
}
