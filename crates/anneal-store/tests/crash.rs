//! Store-level crash-injection tests (the anneal-store proposal §3.3).
//!
//! Pattern: **self-reexec.** The driver spawns this same test binary filtered
//! down to [`crash_helper`], with `ANNEAL_CRASH_HELPER=<label>` selecting an
//! operation and `ANNEAL_CRASH_AFTER=<label>` arming the abort inside it. The
//! child dies at the labeled persistence phase (a deterministic stand-in for
//! `kill -9`); the parent then reopens the store and asserts the recovery
//! invariant: *the store opens, and the crash state is a benign one.*
//!
//! The one-sentence contract under test, per label: after death at that phase,
//! the next run produces a usable store — a complete state, a complete older
//! state, or a cold path — never a dangling reference or a build error.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anneal_core::Digest;
use anneal_store::{Recovered, Store, StoredResult, WarmManifest};

/// The child entry point: performs the labeled operation and dies inside it.
#[test]
fn crash_helper() {
    let Ok(label) = std::env::var("ANNEAL_CRASH_HELPER") else {
        // Driven only when spawned by the drivers below; a plain run of this
        // binary has nothing to do.
        return;
    };
    let root = PathBuf::from(std::env::var("ANNEAL_CRASH_STORE").expect("spawned with a store"));
    let store = Store::open(&root).unwrap();
    match label.as_str() {
        // A blob put dies between data write and rename: orphaned tmp debris.
        "blob-put" => {
            store.cas().put(b"payload").unwrap();
            unreachable!("crash point must fire inside put");
        }
        // An entry insert dies before the rename publishes it.
        "action-insert" => {
            let key = Digest::of(b"key");
            store
                .actions()
                .insert(
                    &key,
                    &StoredResult {
                        exit_code: 0,
                        outputs: BTreeMap::from([("out".to_owned(), Digest::of(b"payload"))]),
                        provenance: None,
                    },
                )
                .unwrap();
            unreachable!("crash point must fire inside insert");
        }
        // A snapshot save dies after the manifest blob, before the index.
        "snapshot-manifest" => {
            let dir = root.join("src-tree");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.txt"), b"state").unwrap();
            store
                .snapshots()
                .save(store.cas(), &Digest::of(b"snap"), &dir)
                .unwrap();
            unreachable!("crash point must fire inside save");
        }
        // A snapshot save dies inside the index write, after the manifest.
        "snapshot-index" => {
            let dir = root.join("src-tree");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.txt"), b"state").unwrap();
            store
                .snapshots()
                .save(store.cas(), &Digest::of(b"snap"), &dir)
                .unwrap();
            unreachable!("crash point must fire inside write_index");
        }
        // BEGIN dies after the commit record is destroyed: an unproven tree.
        "warm-begin" => {
            let guard = store.lock().unwrap();
            let warm = guard.warm(&Digest::of(b"warm"));
            let txn = warm.lock();
            txn.begin().unwrap();
            unreachable!("crash point must fire inside begin");
        }
        // COMMIT dies before the manifest is written: the run is lost, never
        // half-recorded.
        "warm-commit" => {
            let cwd = root.join("warm-cwd");
            std::fs::create_dir_all(&cwd).unwrap();
            std::fs::write(cwd.join("in.txt"), b"content").unwrap();
            let manifest = WarmManifest::record(
                "owner",
                &cwd,
                &BTreeMap::from([(PathBuf::from("in.txt"), Digest::of(b"content"))]),
            )
            .unwrap();
            let guard = store.lock().unwrap();
            let warm = guard.warm(&Digest::of(b"warm"));
            let txn = warm.lock();
            txn.begin().unwrap();
            txn.commit(&manifest).unwrap();
            unreachable!("crash point must fire inside commit");
        }
        other => panic!("unknown helper label {other:?}"),
    }
}

/// Spawn the helper child for `label` against a fresh store; returns its exit
/// status. The child must die by abort.
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

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn blob_put_crash_leaves_an_absent_blob_and_a_reusable_store() {
    let tmp = tempdir();
    let root = tmp.path().join(".anneal");
    die_at("blob-put", &root);
    // The store reopens; the interrupted blob is simply absent.
    let store = Store::open(&root).unwrap();
    assert!(!store.cas().has(&Digest::of(b"payload")));
    // A retry completes.
    let digest = store.cas().put(b"payload").unwrap();
    assert_eq!(digest, Digest::of(b"payload"));
    assert!(store.cas().has(&digest));
}

#[test]
fn action_insert_crash_leaves_no_entry() {
    let tmp = tempdir();
    let root = tmp.path().join(".anneal");
    die_at("action-insert", &root);
    let store = Store::open(&root).unwrap();
    let key = Digest::of(b"key");
    assert_eq!(store.actions().lookup(&key).unwrap(), None);
    // The retry (with output blob present) inserts cleanly.
    let out = store.cas().put(b"payload").unwrap();
    store
        .actions()
        .insert(
            &key,
            &StoredResult {
                exit_code: 0,
                outputs: BTreeMap::from([("out".to_owned(), out)]),
                provenance: None,
            },
        )
        .unwrap();
    assert!(store.actions().lookup(&key).unwrap().is_some());
}

#[test]
fn snapshot_crashes_degrade_to_a_cold_restore() {
    for label in ["snapshot-manifest", "snapshot-index"] {
        let tmp = tempdir();
        let root = tmp.path().join(".anneal");
        die_at(label, &root);
        // The store reopens and the snapshot restores without error — as a
        // cold start (Ok(false)) if the index never published.
        let store = Store::open(&root).unwrap();
        let restored = store
            .snapshots()
            .restore(store.cas(), &Digest::of(b"snap"), &tmp.path().join("out"))
            .unwrap();
        // Either the index never landed (false) or the full generation did
        // (true); both are valid crash states. An error would not be.
        let _ = restored;
        // A retry save completes and restores.
        let dir = root.join("src-tree");
        assert!(store
            .snapshots()
            .save(store.cas(), &Digest::of(b"snap"), &dir)
            .unwrap());
        assert!(store
            .snapshots()
            .restore(store.cas(), &Digest::of(b"snap"), &tmp.path().join("out2"))
            .unwrap());
    }
}

#[test]
fn warm_crashes_leave_an_unproven_tree_and_a_cold_next_run() {
    for label in ["warm-begin", "warm-commit"] {
        let tmp = tempdir();
        let root = tmp.path().join(".anneal");
        die_at(label, &root);
        // The manifest never committed → Absent, and the store reopens.
        let store = Store::open(&root).unwrap();
        assert!(
            store
                .load_warm_manifest(&Digest::of(b"warm"))
                .unwrap()
                .is_absent(),
            "{label}: the crash must leave no committed record"
        );
        // A full BEGIN→COMMIT retry works.
        let cwd = root.parent().unwrap().join("retry-cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("in.txt"), b"content").unwrap();
        let manifest = WarmManifest::record(
            "owner",
            &cwd,
            &BTreeMap::from([(PathBuf::from("in.txt"), Digest::of(b"content"))]),
        )
        .unwrap();
        let guard = store.lock().unwrap();
        let warm = guard.warm(&Digest::of(b"warm"));
        let txn = warm.lock();
        txn.begin().unwrap();
        txn.commit(&manifest).unwrap();
        drop(txn);
        drop(guard);
        assert!(matches!(
            store.load_warm_manifest(&Digest::of(b"warm")).unwrap(),
            Recovered::Clean(_)
        ));
    }
}
