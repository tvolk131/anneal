//! `anneal-store` — the one crate that owns the `.anneal/` directory
//! (`docs/proposals/anneal-store.md`).
//!
//! Layout (§2 of the proposal):
//!
//! ```text
//! .anneal/
//!   store/               # the transport unit — everything content-addressed
//!     format              # layout-version marker (the import gate)
//!     objects/            # CAS blobs
//!     actions/            # action-cache entries
//!     snapshots/          # snapshot index (manifests and contents are blobs)
//!   local/                # never leaves this machine
//!     digest-cache        # (path, mtime, size, ctime, inode) → digest memo
//!     warm/<key16>/       # warm working trees
//!     warm-meta/<key16>/  # warm commit records
//!     sandboxes/          # transient per-action scratch
//!     queries/            # per-identity stable query execution roots
//!     materialized/       # worktree materialization ownership
//!     tmp/                # staging and crash debris
//!   lock                  # the single immortal advisory lock
//! ```
//!
//! Placement rule: **if it is addressed by content, it goes in `store/`; if it
//! references mtimes, absolute paths, PIDs, or machine identity, it goes in
//! `local/`.** `store/` is money (portable; losing it costs only time);
//! `local/` is memory (machine-bound).
//!
//! ## Concurrency (§4)
//!
//! Readers are lock-free at both levels: everything reachable without a guard
//! is immutable-once-published, and consumers fail open on absence. Mutators
//! serialize on one immortal `flock(LOCK_EX)` acquired through
//! [`Store::lock`] → [`WorkspaceGuard`] — the write capability. In-process
//! granularity lives *under* that coarse lock: same-warm-key owners serialize
//! on a per-key mutex held by an RAII [`WarmHandle`]. The in-process locks are
//! sound because the flock exists; there are deliberately no per-key lock
//! files (a lock GC could delete breaks `flock` mutual exclusion).
//!
//! ## Crash safety (§3)
//!
//! `store/` is safe by ordering — per-file atomic renames, references written
//! back-to-front (blobs before entries), idempotent commutative writes; no
//! journal is needed or used. `local/` is safe by detect-and-degrade — the
//! warm manifest doubles as a transaction commit record, and every torn load
//! returns [`Recovered`] so accelerator state degrades instead of erroring.
//! The `ANNEAL_CRASH_AFTER` hook in `anneal-core` pins all of it by test.

mod action_cache;
mod trust;
mod warm;

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anneal_cas::Cas;
use anneal_core::Digest;
use anneal_snapshot::SnapshotStore;

pub use action_cache::{ActionCache, StoredResult};
pub use trust::{CacheTier, EnforcementGrade, Provenance};
pub use warm::{WarmEntry, WarmManifest};

/// The `store/format` marker value. Gates *where files live*; the in-key tags
/// (`anneal-action-v2`, the sandbox version) gate *what entries mean*. Import
/// checks both, and they bump independently.
pub const STORE_FORMAT: &str = "anneal-store-v1";

/// The trust tier of a verification boundary.
///
/// - [`Verify::Stats`] — stat-level checking: cheap, catches absence (the
///   action-cache hit check, the warm drift check).
/// - [`Verify::Hash`] — re-hash bytes against their name: the paranoid tier
///   (import admission; catches power-loss "lying names").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verify {
    Stats,
    Hash,
}

/// The result of loading state that may have been torn by a crash: policy
/// encoded as a type, so "degrade, don't error" for accelerator state is
/// inescapable at every call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovered<T> {
    /// Loaded intact.
    Clean(T),
    /// Loaded with something dropped — `note` says what. Using the value is
    /// sound because the dropped parts fail toward re-work, never staleness.
    Degraded { value: T, note: String },
    /// No usable state (absent or header-corrupt): the caller takes the cold
    /// path.
    Absent,
}

impl<T> Recovered<T> {
    pub fn into_value(self) -> Option<T> {
        match self {
            Recovered::Clean(v) | Recovered::Degraded { value: v, .. } => Some(v),
            Recovered::Absent => None,
        }
    }

    pub fn is_absent(&self) -> bool {
        matches!(self, Recovered::Absent)
    }
}

/// The open `.anneal` store: handles for the transportable half (`store/`) and
/// the machine-bound half (`local/`). Cheap to clone — all state is shared.
///
/// `Store::open` is **read-only**: it creates the layout and opens handles but
/// performs no recovery and takes no lock. Mutation — recovery sweeps and
/// every write — happens behind [`Store::lock`].
pub struct Store {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    cas: Cas,
    actions: ActionCache,
    snapshots: SnapshotStore,
    /// The transportable half (`store/`).
    store_root: PathBuf,
    /// The machine-bound half (`local/`).
    local_root: PathBuf,
    lock_path: PathBuf,
    /// Per-warm-key serialization locks: same-key owners share one warm dir
    /// and serialize on it; different keys run free.
    warm_locks: Mutex<HashMap<Digest, Arc<Mutex<()>>>>,
    /// Whether this process currently holds the workspace flock. Guards the
    /// classic `flock` footgun: a second `open` + `LOCK_EX` from the same
    /// process deadlocks rather than recursing, so re-locking is a defined
    /// error instead.
    lock_held: AtomicBool,
}

impl Clone for Store {
    fn clone(&self) -> Self {
        Store {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Store {
    /// Open (creating if necessary) the store rooted at `root` — the `.anneal`
    /// directory itself. No legacy-layout handling: an older layout is simply
    /// absent from the new paths and re-populates (a breaking change accepted
    /// in the proposal, §9.3).
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Store> {
        let root = root.into();
        let store_root = root.join("store");
        let local_root = root.join("local");
        for dir in [
            store_root.join("objects"),
            store_root.join("actions"),
            store_root.join("snapshots"),
            local_root.join("warm"),
            local_root.join("warm-meta"),
            local_root.join("sandboxes"),
            local_root.join("queries"),
            local_root.join("tmp"),
        ] {
            fs::create_dir_all(dir)?;
        }
        // The format marker: written once; a mismatched marker is a hard error
        // (the store was written by a layout this binary does not know).
        let format_path = store_root.join("format");
        match fs::read_to_string(&format_path) {
            Ok(found) if found.trim() == STORE_FORMAT => {}
            Ok(found) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "store format mismatch at {}: found {:?}, this binary expects \
                         {STORE_FORMAT:?}",
                        format_path.display(),
                        found.trim()
                    ),
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::write(&format_path, STORE_FORMAT)?;
            }
            Err(e) => return Err(e),
        }

        let cas = Cas::open_split(store_root.join("objects"), local_root.join("digest-cache"))?;
        let actions = ActionCache::open(store_root.join("actions"))?;
        let snapshots = SnapshotStore::open(store_root.join("snapshots"))?;
        Ok(Store {
            inner: Arc::new(StoreInner {
                cas,
                actions,
                snapshots,
                lock_path: root.join("lock"),
                store_root,
                local_root,
                warm_locks: Mutex::new(HashMap::new()),
                lock_held: AtomicBool::new(false),
            }),
        })
    }

    /// The CAS — blob storage shared by every zone.
    pub fn cas(&self) -> &Cas {
        &self.inner.cas
    }

    /// The action cache (read view; writes go through a [`WorkspaceGuard`]).
    pub fn actions(&self) -> &ActionCache {
        &self.inner.actions
    }

    /// The snapshot store.
    pub fn snapshots(&self) -> &SnapshotStore {
        &self.inner.snapshots
    }

    /// The transportable half's root (`store/`).
    pub fn store_root(&self) -> &Path {
        &self.inner.store_root
    }

    /// The machine-bound half's root (`local/`).
    pub fn local_root(&self) -> &Path {
        &self.inner.local_root
    }

    /// Transient per-action scratch sandboxes (deletable at any time nothing
    /// is running).
    pub fn sandboxes_root(&self) -> PathBuf {
        self.inner.local_root.join("sandboxes")
    }

    /// Stable per-identity roots for query execution.
    pub fn queries_root(&self) -> PathBuf {
        self.inner.local_root.join("queries")
    }

    /// The warm working tree for a snapshot key.
    pub fn warm_dir(&self, key: &Digest) -> PathBuf {
        self.inner.local_root.join("warm").join(&key.to_hex()[..16])
    }

    /// The warm commit record for a snapshot key.
    pub fn warm_manifest_path(&self, key: &Digest) -> PathBuf {
        self.inner
            .local_root
            .join("warm-meta")
            .join(&key.to_hex()[..16])
            .join("inputs")
    }

    /// Load the warm commit record through the tolerant path: torn or absent
    /// records degrade (`Recovered`), never error — accelerator state must not
    /// fail a build.
    pub fn load_warm_manifest(&self, key: &Digest) -> io::Result<Recovered<WarmManifest>> {
        warm::load_warm_manifest(&self.warm_manifest_path(key))
    }

    /// The per-key serialization lock, created on first use. Keyed by
    /// [`Digest`]: warm-tree transactions and query-identity serialization
    /// both use it (their key spaces are disjoint by construction). Different
    /// keys never contend.
    pub fn key_lock(&self, key: &Digest) -> Arc<Mutex<()>> {
        self.inner
            .warm_locks
            .lock()
            .unwrap()
            .entry(*key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Take the workspace write capability: `flock(LOCK_EX)` on the single
    /// immortal `.anneal/lock`, then **boot recovery** (sweep crash-orphaned
    /// sandboxes, query roots, and tmp debris). Held until every clone of the
    /// returned guard drops; `flock` is also released on process death, so a
    /// crash leaves no stale lock.
    ///
    /// A second `lock()` while one is held in this process is a defined error,
    /// not a deadlock. If another *process* holds the lock, this blocks (with
    /// a one-line note about the holder).
    pub fn lock(&self) -> io::Result<WorkspaceGuard> {
        if self.inner.lock_held.swap(true, Ordering::SeqCst) {
            return Err(io::Error::other(
                "workspace lock already held by this process",
            ));
        }
        match self.acquire_flock() {
            Ok(file) => {
                self.recover()?;
                Ok(WorkspaceGuard {
                    store: self.clone(),
                    lock: Some(Arc::new(LockHeld {
                        _file: file,
                        clones: AtomicUsize::new(1),
                    })),
                })
            }
            Err(e) => {
                self.inner.lock_held.store(false, Ordering::SeqCst);
                Err(e)
            }
        }
    }

    /// Open the lock file and take the exclusive flock, blocking on contention.
    fn acquire_flock(&self) -> io::Result<fs::File> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.inner.lock_path)?;
        let fd = file.as_raw_fd();
        // SAFETY: `fd` is a valid open descriptor owned by `file` for the
        // call's duration.
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(err);
            }
            // Contended: report who holds it (best-effort) and block.
            let store_dir = self
                .inner
                .lock_path
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| self.inner.lock_path.display().to_string());
            match fs::read_to_string(&self.inner.lock_path)
                .ok()
                .filter(|s| !s.trim().is_empty())
            {
                Some(holder) => eprintln!(
                    "Blocking: waiting for another anneal process (PID {}) on {store_dir}",
                    holder.trim()
                ),
                None => eprintln!("Blocking: waiting for another anneal process on {store_dir}"),
            }
            if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // We hold the lock — record our PID for the next waiter's diagnostics.
        // Best-effort; a separate open() doesn't disturb the flock held on
        // `file`'s descriptor.
        let _ = fs::write(&self.inner.lock_path, format!("{}\n", std::process::id()));
        Ok(file)
    }

    /// Boot recovery (§3.2): sweep crash-orphaned scratch. Safe under the
    /// freshly-held lock — no other mutator exists, and the lock-free readers
    /// never touch these directories.
    fn recover(&self) -> io::Result<()> {
        for dir in [
            self.sandboxes_root(),
            self.queries_root(),
            self.inner.local_root.join("tmp"),
        ] {
            let Ok(children) = fs::read_dir(&dir) else {
                continue;
            };
            for child in children.flatten() {
                let _ = fs::remove_dir_all(child.path());
            }
        }
        Ok(())
    }
}

/// One held workspace flock: the file descriptor keeps the `flock` alive (the
/// field is read by `Drop` semantics, not by code), and `clones` counts guards
/// sharing it — the flag clears when the last drops.
struct LockHeld {
    _file: fs::File,
    clones: AtomicUsize,
}

/// The workspace **write capability**: handed out by [`Store::lock`], and the
/// only route to mutating the store. Clones share the same flock; the lock is
/// released when the last clone drops.
///
/// A detached guard (no flock) exists for embedders that manage cross-process
/// exclusion themselves — see [`WorkspaceGuard::detached`]. Mutating methods
/// take `&self` (not `&mut`): the guard proves *this process* holds the lock,
/// while per-warm-key granularity lives in the [`WarmHandle`] below.
pub struct WorkspaceGuard {
    store: Store,
    lock: Option<Arc<LockHeld>>,
}

impl WorkspaceGuard {
    /// A guard that holds no flock. For embedders (library use, executor
    /// tests) where cross-process exclusion is managed by the caller or
    /// unnecessary; the CLI always takes the real [`Store::lock`].
    pub fn detached(store: &Store) -> WorkspaceGuard {
        WorkspaceGuard {
            store: store.clone(),
            lock: None,
        }
    }

    /// The write path to the action cache.
    pub fn actions(&self) -> &ActionCache {
        self.store.actions()
    }

    /// The RAII handle to one warm key's transaction: acquiring the inner
    /// lock serializes same-key owners while different keys run free.
    pub fn warm(&self, key: &Digest) -> WarmHandle {
        WarmHandle {
            store: self.store.clone(),
            key: *key,
            mutex: self.store.key_lock(key),
        }
    }

    /// The store this guard writes (read view included).
    pub fn store(&self) -> &Store {
        &self.store
    }
}

impl Clone for WorkspaceGuard {
    fn clone(&self) -> Self {
        if let Some(held) = &self.lock {
            held.clones.fetch_add(1, Ordering::SeqCst);
        }
        WorkspaceGuard {
            store: self.store.clone(),
            lock: self.lock.clone(),
        }
    }
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        if let Some(held) = &self.lock {
            // The last clone clears the in-process held flag; dropping the
            // `Arc` (and with it the fd) releases the flock.
            if held.clones.fetch_sub(1, Ordering::SeqCst) == 1 {
                self.store.inner.lock_held.store(false, Ordering::SeqCst);
            }
        }
    }
}

/// A per-warm-key transaction handle: [`WarmHandle::lock`] acquires the key's
/// serialization mutex for the handle's lifetime (RAII — the BEGIN/sync/run/
/// COMMIT sequence cannot forget it).
pub struct WarmHandle {
    store: Store,
    key: Digest,
    mutex: Arc<Mutex<()>>,
}

impl WarmHandle {
    pub fn key(&self) -> &Digest {
        &self.key
    }

    /// Whether the key's mutex is free right now — diagnostics and tests.
    pub fn mutex_try_lock(&self) -> bool {
        self.mutex.try_lock().is_ok()
    }

    /// Begin the transaction: holds the key's mutex until the returned
    /// [`WarmTxn`] drops.
    pub fn lock(&self) -> WarmTxn<'_> {
        WarmTxn {
            handle: self,
            _guard: self.mutex.lock().unwrap(),
        }
    }
}

/// An in-progress warm transaction: the key's mutex is held; `begin`/`commit`
/// drive the manifest-as-commit-record protocol (§3.2).
pub struct WarmTxn<'a> {
    handle: &'a WarmHandle,
    _guard: std::sync::MutexGuard<'a, ()>,
}

impl WarmTxn<'_> {
    /// The warm working tree this transaction owns.
    pub fn warm_dir(&self) -> PathBuf {
        self.handle.store.warm_dir(&self.handle.key)
    }

    fn manifest_path(&self) -> PathBuf {
        self.handle.store.warm_manifest_path(&self.handle.key)
    }

    /// BEGIN: destroy the commit record *before* any mutation of the tree.
    /// From here until [`WarmTxn::commit`], the tree is officially unproven —
    /// a crash means the next run cold-populates rather than trusting a
    /// half-synced tree.
    pub fn begin(&self) -> io::Result<()> {
        match fs::remove_file(self.manifest_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        // Crash-injection point: after the record is destroyed, before the
        // tree is touched — the "unproven tree" crash state.
        anneal_core::crash_point("warm-begin");
        Ok(())
    }

    /// COMMIT (on success only): atomically publish the new baseline. The
    /// manifest's presence marks the tree clean.
    pub fn commit(&self, manifest: &WarmManifest) -> io::Result<()> {
        // Crash-injection point: after a successful run, before the record is
        // written — the run is lost to a crash, never half-recorded.
        anneal_core::crash_point("warm-commit");
        manifest.save_atomic(&self.manifest_path())
    }

    /// Remove the commit record without holding the transaction lock — the
    /// verifier's `fresh = true` path.
    pub fn discard_record(&self) {
        let _ = fs::remove_file(self.manifest_path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tmp: &tempfile::TempDir) -> Store {
        Store::open(tmp.path().join(".anneal")).unwrap()
    }

    #[test]
    fn open_creates_the_layout_and_format_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(&tmp);
        let root = tmp.path().join(".anneal");
        assert_eq!(
            fs::read_to_string(root.join("store/format")).unwrap(),
            STORE_FORMAT
        );
        for dir in [
            s.store_root().join("objects"),
            s.store_root().join("actions"),
            s.store_root().join("snapshots"),
            s.local_root().join("warm"),
            s.local_root().join("warm-meta"),
            s.local_root().join("sandboxes"),
            s.local_root().join("queries"),
            s.local_root().join("tmp"),
        ] {
            assert!(dir.is_dir(), "{} must exist", dir.display());
        }
    }

    #[test]
    fn mismatched_format_marker_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".anneal");
        fs::create_dir_all(root.join("store")).unwrap();
        fs::write(root.join("store/format"), "anneal-store-v0").unwrap();
        let err = Store::open(&root)
            .err()
            .expect("format mismatch must error");
        assert!(err.to_string().contains("format mismatch"));
    }

    #[test]
    fn second_lock_in_one_process_is_a_defined_error_not_a_deadlock() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(&tmp);
        let _guard = s.lock().unwrap();
        let err = s.lock().err().expect("re-lock must error");
        assert!(err.to_string().contains("already held"));
        // Drop, then re-locking works.
        drop(_guard);
        assert!(s.lock().is_ok());
    }

    #[test]
    fn guard_clones_release_the_lock_only_when_the_last_drops() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(&tmp);
        let guard = s.lock().unwrap();
        let clone = guard.clone();
        drop(guard);
        // The clone still holds it.
        assert!(s.lock().is_err());
        drop(clone);
        assert!(s.lock().is_ok());
    }

    #[test]
    fn detached_guard_never_holds_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(&tmp);
        let _detached = WorkspaceGuard::detached(&s);
        assert!(s.lock().is_ok(), "a detached guard must not hold the flock");
    }

    #[test]
    fn recovery_sweeps_crash_orphaned_scratch() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(&tmp);
        let orphan = s.sandboxes_root().join("dead-beef-123");
        fs::create_dir_all(&orphan).unwrap();
        fs::write(orphan.join("partial"), b"debris").unwrap();
        let query_orphan = s.queries_root().join("abc");
        fs::create_dir_all(&query_orphan).unwrap();

        let _guard = s.lock().unwrap();
        assert!(!orphan.exists(), "recovery must sweep sandbox debris");
        assert!(!query_orphan.exists(), "recovery must sweep query roots");
    }

    #[test]
    fn warm_handle_serializes_same_key_and_frees_different_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(&tmp);
        let guard = s.lock().unwrap();
        let key = Digest::of(b"key");
        let warm = guard.warm(&key);
        let txn = warm.lock();
        // Same key: a second transaction cannot start while the first lives.
        assert!(!guard.warm(&key).mutex_try_lock());
        // A different key runs free.
        let other_handle = guard.warm(&Digest::of(b"other"));
        let other = other_handle.lock();
        drop(other);
        drop(other_handle);
        drop(txn);
        drop(warm);
        assert!(guard.warm(&key).mutex_try_lock());
    }
}
