//! End-to-end test of warm-sandbox reuse (docs/sandboxing.md §5) through the executor.
//!
//! A `SnapshotBased` action concatenates two declared inputs into its output. With warm
//! reuse on, the snapshot owner keeps its working tree in place across builds and syncs
//! only the changed inputs. The headline assertion is **correctness under a revert**: an
//! input that reverts to earlier content pulls an *old* CAS blob, which — if placed as a
//! stale-mtime hardlink/clone — cargo-style mtime freshness would silently miss. The sync
//! must instead deliver the reverted content with a fresh mtime, so the build sees it.

use std::path::PathBuf;

use anneal_core::Digest;
use anneal_exec::{Action, Executor, LocalExecutor};

/// `cat a.txt b.txt > out.txt` as a snapshot owner sharing one stable snapshot key, so
/// every build maps to the same warm working tree.
fn concat_action(a: Digest, b: Digest, snapshot_key: Digest) -> Action {
    Action::builder("concat", ["/bin/sh", "-c", "mkdir -p cache && cat a.txt b.txt > out.txt"])
        .input("a", "a.txt", a)
        .input("b", "b.txt", b)
        .output("out", "out.txt")
        .snapshot(snapshot_key, vec![PathBuf::from("cache")])
        .build()
}

#[test]
fn warm_reuse_stays_correct_across_a_revert() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join(".anneal");
    let exec = LocalExecutor::new(&store).unwrap().warm_reuse();
    let skey = Digest::of(b"warm-reuse-test-key");

    let out = |a: &[u8], b: &[u8]| -> Vec<u8> {
        let da = exec.cas().put(a).unwrap();
        let db = exec.cas().put(b).unwrap();
        let result = exec.execute(&concat_action(da, db, skey)).unwrap();
        assert!(result.success(), "build failed (exit {})", result.exit_code);
        exec.cas().get(result.outputs.get("out").unwrap()).unwrap().unwrap()
    };

    // 1) cold-populate the warm dir.
    assert_eq!(out(b"A1", b"B1"), b"A1B1");
    // 2) reuse: `a` changes A1 -> A2 (synced in place; b untouched).
    assert_eq!(out(b"A2", b"B1"), b"A2B1");
    // 3) reuse + THE HAZARD: `a` reverts to A1 (an old blob) while `b` changes to B2, so
    //    the action key is novel (cache miss → run_warm runs). If the reverted `a` were
    //    placed with its old blob's stale mtime, the build would still see A2 → "A2B2".
    //    The fresh-mtime sync makes it "A1B2".
    assert_eq!(out(b"A1", b"B2"), b"A1B2", "a reverted to A1 must not be silently stale as A2");

    // The warm working tree persisted (no teardown) and reflects the last synced state.
    let tag = &skey.to_hex()[..16];
    let warm_dir = store.join("warm").join(tag);
    assert!(warm_dir.exists(), "warm working tree must persist between builds");
    assert_eq!(std::fs::read(warm_dir.join("a.txt")).unwrap(), b"A1");
    assert_eq!(std::fs::read(warm_dir.join("b.txt")).unwrap(), b"B2");
    // The manifest (commit record) is present after a clean build.
    assert!(store.join("warm-meta").join(tag).join("inputs").exists());
}

#[test]
fn warm_and_cold_paths_agree() {
    // Correctness-neutrality at the integration level: the same inputs produce the same
    // output whether built warm-reuse or through the default fresh-sandbox path.
    let inputs: [(&[u8], &[u8]); 2] = [(b"hello", b"world"), (b"x", b"y")];
    let skey = Digest::of(b"agreement-key");

    let run = |warm: bool| -> Vec<Vec<u8>> {
        let tmp = tempfile::tempdir().unwrap();
        let mut exec = LocalExecutor::new(tmp.path().join(".anneal")).unwrap();
        if warm {
            exec = exec.warm_reuse();
        }
        inputs
            .iter()
            .map(|(a, b)| {
                let da = exec.cas().put(a).unwrap();
                let db = exec.cas().put(b).unwrap();
                let r = exec.execute(&concat_action(da, db, skey)).unwrap();
                exec.cas().get(r.outputs.get("out").unwrap()).unwrap().unwrap()
            })
            .collect()
    };

    assert_eq!(run(true), run(false), "warm reuse must be output-identical to the cold path");
}
