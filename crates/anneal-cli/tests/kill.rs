//! End-to-end interruption tests through the real `anneal` binary: a build
//! killed mid-flight by crash injection (`ANNEAL_CRASH_AFTER=<label>`) must
//! leave a store the *next* run uses successfully, producing the same declared
//! output as an uninterrupted build (the anneal-store proposal §3.3).
//!
//! Fixtures use `genrule` with the `deterministic` opt-in (so the build has a
//! cacheable action to die inside) and need no language toolchain beyond the
//! base runtime.

use std::path::Path;
use std::process::{Command, Output};

use anneal_core::Digest;

fn anneal(root: &Path, args: &[&str], crash_after: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_anneal"));
    cmd.args(args)
        .arg("--workspace-root")
        .arg(root)
        .env_remove("ANNEAL_CRASH_AFTER");
    if let Some(label) = crash_after {
        cmd.env("ANNEAL_CRASH_AFTER", label);
    }
    cmd.output().expect("run anneal")
}

fn workspace(build: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("BUILD"), build).unwrap();
    tmp
}

const DETERMINISTIC_GENRULE: &str = "genrule(\n\
     \x20   name = \"gen\",\n\
     \x20   srcs = [\"in.txt\"],\n\
     \x20   outs = [\"out.txt\"],\n\
     \x20   cmd = \"tr a-z A-Z < $(SRCS) > $(OUTS)\",\n\
     \x20   deterministic = True,\n\
     )\n";

/// Where a CAS blob lives under the store layout.
fn blob_path(root: &Path, digest: &Digest) -> std::path::PathBuf {
    let hex = digest.to_hex();
    root.join(".anneal/store/objects")
        .join(&hex[..2])
        .join(&hex[2..])
}

#[test]
fn build_killed_mid_flight_recovers_to_the_same_output() {
    // `tr a-z A-Z` on the fixture input — the bytes the declared output must
    // hold after recovery, byte-identical to an uninterrupted build.
    let expected = Digest::of(b"HELLO KILL\n");

    let ws = workspace(DETERMINISTIC_GENRULE);
    std::fs::write(ws.path().join("pkg/in.txt"), b"hello kill\n").unwrap();

    // Kill a build at the action-cache insert: mid-run, after execution, before
    // the entry publishes.
    let killed = anneal(ws.path(), &["build", "//pkg:gen"], Some("action-insert"));
    assert!(
        !killed.status.success(),
        "the injected crash must kill the build"
    );

    // The next run recovers: it succeeds…
    let recovered = anneal(ws.path(), &["build", "//pkg:gen"], None);
    assert!(
        recovered.status.success(),
        "recovered build failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    // …and its declared output is the exact expected content.
    assert!(
        blob_path(ws.path(), &expected).is_file(),
        "the recovered build must have produced the correct output blob"
    );

    // …and the recovered store serves an exact hit afterwards — proving the
    // entry *and* its blobs landed intact (a torn entry would fail open to a
    // re-run, not report CACHED).
    let again = anneal(ws.path(), &["build", "//pkg:gen"], None);
    assert!(again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stdout).contains("CACHED"),
        "the post-crash store must serve an exact hit"
    );
}
