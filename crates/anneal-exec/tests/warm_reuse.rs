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
    Action::builder(
        "concat",
        [
            "/bin/sh",
            "-c",
            "mkdir -p cache && cat a.txt b.txt > out.txt",
        ],
    )
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
    let exec = LocalExecutor::new(&store).unwrap(); // warm reuse on by default
    let skey = Digest::of(b"warm-reuse-test-key");

    let out = |a: &[u8], b: &[u8]| -> Vec<u8> {
        let da = exec.cas().put(a).unwrap();
        let db = exec.cas().put(b).unwrap();
        let result = exec.execute(&concat_action(da, db, skey)).unwrap();
        assert!(result.success(), "build failed (exit {})", result.exit_code);
        exec.cas()
            .get(result.outputs.get("out").unwrap())
            .unwrap()
            .unwrap()
    };

    // 1) cold-populate the warm dir.
    assert_eq!(out(b"A1", b"B1"), b"A1B1");
    // 2) reuse: `a` changes A1 -> A2 (synced in place; b untouched).
    assert_eq!(out(b"A2", b"B1"), b"A2B1");
    // 3) reuse + THE HAZARD: `a` reverts to A1 (an old blob) while `b` changes to B2, so
    //    the action key is novel (cache miss → run_warm runs). If the reverted `a` were
    //    placed with its old blob's stale mtime, the build would still see A2 → "A2B2".
    //    The fresh-mtime sync makes it "A1B2".
    assert_eq!(
        out(b"A1", b"B2"),
        b"A1B2",
        "a reverted to A1 must not be silently stale as A2"
    );

    // The warm working tree persisted (no teardown) and reflects the last synced state.
    let tag = &skey.to_hex()[..16];
    let warm_dir = store.join("warm").join(tag);
    assert!(
        warm_dir.exists(),
        "warm working tree must persist between builds"
    );
    assert_eq!(std::fs::read(warm_dir.join("a.txt")).unwrap(), b"A1");
    assert_eq!(std::fs::read(warm_dir.join("b.txt")).unwrap(), b"B2");
    // The manifest (commit record) is present after a clean build.
    assert!(store.join("warm-meta").join(tag).join("inputs").exists());
}

#[test]
fn private_snapshot_skips_cas_save_under_warm_reuse_but_shared_saves() {
    // §5.8.1: under warm reuse, a *private* snapshot owner must NOT write the snapshot to
    // the CAS (the warm dir is its only live copy), while a *shared* owner must.
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join(".anneal");
    let exec = LocalExecutor::new(&store).unwrap(); // warm reuse on by default
    let index = |k: &Digest| store.join("snapshots").join("index").join(k.to_hex());
    let cmd = [
        "/bin/sh",
        "-c",
        "mkdir -p cache && echo m > cache/m && cp in.txt out.txt",
    ];
    // Distinct input content per action so they have distinct action digests — otherwise
    // the cache key (which excludes snapshot_key/shared/name) would collide and the second
    // would cache-hit the first instead of running.
    let make = |tag_name: &str, content: &[u8], key: Digest, private: bool| -> Action {
        let blob = exec.cas().put(content).unwrap();
        let b = Action::builder(tag_name, cmd)
            .input("in", "in.txt", blob)
            .output("out", "out.txt");
        if private {
            b.snapshot_private(key, vec![PathBuf::from("cache")])
                .build()
        } else {
            b.snapshot(key, vec![PathBuf::from("cache")]).build()
        }
    };

    let priv_key = Digest::of(b"private-snapshot-key");
    assert!(exec
        .execute(&make("priv", b"private-input", priv_key, true))
        .unwrap()
        .success());
    assert!(
        !index(&priv_key).exists(),
        "private snapshot must not be saved to the CAS under warm reuse"
    );
    // ...but the warm dir + commit manifest exist, so reuse still works.
    let tag = &priv_key.to_hex()[..16];
    assert!(
        store.join("warm").join(tag).join("out.txt").exists(),
        "warm tree persists"
    );
    assert!(
        store.join("warm-meta").join(tag).join("inputs").exists(),
        "commit manifest written"
    );

    let shared_key = Digest::of(b"shared-snapshot-key");
    assert!(exec
        .execute(&make("shared", b"shared-input", shared_key, false))
        .unwrap()
        .success());
    assert!(
        index(&shared_key).exists(),
        "shared snapshot must be saved to the CAS for consumers"
    );
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
        if !warm {
            exec = exec.warm_reuse(false); // warm is the default; this is the fresh-path contrast
        }
        inputs
            .iter()
            .map(|(a, b)| {
                let da = exec.cas().put(a).unwrap();
                let db = exec.cas().put(b).unwrap();
                let r = exec.execute(&concat_action(da, db, skey)).unwrap();
                exec.cas()
                    .get(r.outputs.get("out").unwrap())
                    .unwrap()
                    .unwrap()
            })
            .collect()
    };

    assert_eq!(
        run(true),
        run(false),
        "warm reuse must be output-identical to the cold path"
    );
}
