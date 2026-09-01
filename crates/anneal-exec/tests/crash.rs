//! Executor-level crash-injection tests (the anneal-store proposal §3.3): the
//! warm-transaction phases and the materialize write, killed mid-flight in a
//! real child process and then **re-run to identical declared outputs** — the
//! one-sentence invariant: *for every crash point, the next run produces
//! identical declared outputs, whether it recovers warm or falls back to cold.*
//!
//! Self-reexec pattern: the driver spawns this binary filtered to
//! [`crash_helper`], with `ANNEAL_CRASH_HELPER=<label>` selecting the scenario
//! and `ANNEAL_CRASH_AFTER=<label>` arming the abort.

use std::path::{Path, PathBuf};
use std::process::Command;

use anneal_core::Digest;
use anneal_exec::materialize::MaterializeStore;
use anneal_exec::{Action, Executor, LocalExecutor};

mod support;

/// `cp in.txt out.txt` as a private-snapshot owner: the warm-tree shape.
fn copy_action(input: Digest, skey: Digest) -> Action {
    support::shell_action("copy", "mkdir -p cache && cp in.txt out.txt")
        .source_input("in", "in.txt", input)
        .output("out", "out.txt")
        .snapshot_private(skey, vec![PathBuf::from("cache")])
        .build()
}

/// Stage `content` into the CAS, build with it, and return the captured output.
fn out_bytes(exec: &LocalExecutor, content: &[u8], skey: Digest) -> Vec<u8> {
    let input = exec.cas().put(content).unwrap();
    let result = exec.execute(&copy_action(input, skey)).unwrap();
    assert!(result.success(), "build failed (exit {})", result.exit_code);
    exec.cas()
        .get(result.outputs.get("out").unwrap())
        .unwrap()
        .unwrap()
}

/// The child entry point: runs a scenario that dies at its labeled phase.
#[test]
fn crash_helper() {
    let Ok(label) = std::env::var("ANNEAL_CRASH_HELPER") else {
        // Driven only when spawned by the drivers below; a plain run of this
        // binary has nothing to do.
        return;
    };
    let root = PathBuf::from(std::env::var("ANNEAL_CRASH_STORE").expect("spawned with a store"));
    let exec = LocalExecutor::new(&root).unwrap();
    let skey = Digest::of(b"crash-warm-key");
    match label.as_str() {
        // Dies in BEGIN, after the commit record is destroyed.
        "warm-begin" => {
            out_bytes(&exec, b"v1", skey); // commit a baseline first
            let _ = out_bytes(&exec, b"v2", skey); // reuse → BEGIN → abort
            unreachable!("crash point must fire inside begin");
        }
        // Dies placing a changed input during the reuse sync.
        "warm-input-place" => {
            out_bytes(&exec, b"v1", skey);
            let _ = out_bytes(&exec, b"v2", skey); // sync → place_fresh → abort
            unreachable!("crash point must fire inside place_fresh");
        }
        // Dies at COMMIT, after a successful run, before the record lands.
        "warm-commit" => {
            let _ = out_bytes(&exec, b"v1", skey); // cold run → COMMIT → abort
            unreachable!("crash point must fire inside commit");
        }
        // Dies after the tree write, before the ownership manifest publishes.
        "materialize-write" => {
            let digest = exec.cas().put(b"{\"x\":1}").unwrap();
            let mut store =
                MaterializeStore::open(root.join("local"), root.parent().unwrap()).unwrap();
            store
                .apply(
                    "//pkg:t",
                    &[(PathBuf::from("gen/config.json"), digest)],
                    exec.cas(),
                    false,
                )
                .unwrap();
            unreachable!("crash point must fire inside save");
        }
        other => panic!("unknown helper label {other:?}"),
    }
}

fn die_at(label: &str, root: &Path) -> std::process::ExitStatus {
    let status = Command::new(std::env::current_exe().unwrap())
        .arg("crash_helper")
        .env("ANNEAL_CRASH_HELPER", label)
        .env("ANNEAL_CRASH_AFTER", label)
        .env("ANNEAL_CRASH_STORE", root)
        .status()
        .expect("spawn crash helper");
    assert!(
        !status.success(),
        "the child must die at the crash point, not succeed"
    );
    status
}

#[test]
fn warm_crash_recovery_produces_identical_outputs() {
    for label in ["warm-begin", "warm-input-place", "warm-commit"] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".anneal");

        // The child runs the v1 build (and for the reuse labels, commits it)
        // and dies mid-flight on the v2 build.
        die_at(label, &root);

        // The invariant: re-running v2 produces the identical declared output,
        // whether the warm tree was recovered or cold-repopulated.
        let exec = LocalExecutor::new(&root).unwrap();
        let skey = Digest::of(b"crash-warm-key");
        let out = out_bytes(&exec, b"v2", skey);
        assert_eq!(out, b"v2", "{label}: recovered run must produce v2");

        // And the tree is committed again — a further reuse is clean.
        assert!(exec.store().warm_manifest_path(&skey).exists());
        assert_eq!(out_bytes(&exec, b"v2", skey), b"v2");
    }
}

#[test]
fn materialize_crash_recovery_leaves_a_reconcilable_state() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(".anneal");
    die_at("materialize-write", &root);

    // The tree file landed but the ownership record did not publish — the
    // store reopens empty and a re-apply adopts the identical content rather
    // than erroring (never-clobber: identical bytes are safe to adopt).
    let exec = LocalExecutor::new(&root).unwrap();
    let digest = exec.cas().put(b"{\"x\":1}").unwrap();
    let mut store = MaterializeStore::open(root.join("local"), tmp.path()).unwrap();
    assert!(store.entries().is_empty(), "the record never published");
    let report = store
        .apply(
            "//pkg:t",
            &[(PathBuf::from("gen/config.json"), digest)],
            exec.cas(),
            false,
        )
        .unwrap();
    assert_eq!(report.unchanged, vec![PathBuf::from("gen/config.json")]);
    assert!(report.refused.is_empty());
}
