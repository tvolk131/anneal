//! `anneal-cas` — the content-addressed store (§3.4).
//!
//! A deep module: the public surface is `open` / [`Cas::put`] / [`Cas::get`] /
//! [`Cas::has`] / [`Cas::link_into`]. Everything about *how* blobs are stored —
//! the on-disk directory layout, prefix sharding, atomic writes, and the
//! hardlink-vs-copy fallback — is hidden.
//!
//! ## Why `link_into` lives here, not in the materializer
//!
//! The materializer (`anneal-exec`) decides *which* inputs go *where* in a sandbox.
//! But getting bytes from a CAS blob onto the filesystem cheaply depends on the
//! blob's real on-disk path and on which volume the store lives — both private to
//! this module. So the CAS owns the *mechanism* ([`Cas::link_into`], including the
//! cross-filesystem copy fallback proven necessary in Spike B), and never learns
//! about sandboxes. The materializer never sees a path. This keeps the storage
//! layout fully hidden behind a narrow interface.
//!
//! ## Protecting the store from materialized inputs
//!
//! A materialized input must not be a route to corrupting the immutable store.
//!
//! * **macOS (APFS):** inputs are placed with `clonefile(2)` — a copy-on-write clone
//!   on a *separate inode*. A write to the input COWs and never touches the CAS blob,
//!   and because it is a distinct inode we can also mark it read-only (`0444`) without
//!   affecting the blob. This also sidesteps the per-inode hardlink-limit concern of
//!   Spike B (no shared inode at all).
//! * **Linux/other:** inputs are hardlinked (shared inode); strict read-only
//!   enforcement is the future kernel bind-mount path, so we do not chmod the shared
//!   inode here.
//!
//! Both fall back to a copy across filesystems / on non-CoW volumes.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use anneal_core::Digest;

/// `EXDEV` ("cross-device link") errno — 18 on Linux. A hardlink across filesystems
/// fails with this; we fall back to a copy (Spike B finding §4).
#[cfg(not(target_os = "macos"))]
const EXDEV: i32 = 18;

/// Disambiguates temp file names within a process so concurrent `put`s don't collide.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How a blob was placed into the destination by [`Cas::link_into`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// Hardlinked — O(1), shares the inode with the CAS blob (Linux common case).
    Hardlinked,
    /// Copy-on-write clone — O(1), a separate inode sharing storage (APFS). A write
    /// to the input COWs and cannot corrupt the CAS blob.
    Cloned,
    /// Copied — destination was on a different filesystem / non-CoW volume.
    Copied,
}

/// One `(file-identity → digest)` record. The identity is `(mtime, size)`; a match
/// lets [`Cas::ingest_file`] return the digest without re-reading or re-hashing the
/// file body. Content-blind, exactly as cargo's own fingerprint is (see `ingest_file`).
#[derive(Debug, Clone, Copy)]
struct CacheEntry {
    mtime_nanos: u128,
    size: u64,
    digest: Digest,
}

/// The in-memory digest cache, loaded from `<root>/digest-cache` on open and persisted
/// (atomically) on flush/drop. `dirty` avoids rewriting an unchanged cache.
#[derive(Default)]
struct DigestCache {
    entries: HashMap<PathBuf, CacheEntry>,
    dirty: bool,
}

/// A content-addressed store rooted at a directory.
pub struct Cas {
    /// `<root>/objects` — all blobs live under here, prefix-sharded.
    objects: PathBuf,
    /// `<root>/digest-cache` — the persisted `(path,mtime,size) → digest` table.
    cache_path: PathBuf,
    /// The digest cache (see [`Cas::ingest_file`]). A local, rebuildable optimization.
    digest_cache: Mutex<DigestCache>,
    /// Count of files actually read+hashed (digest-cache misses) — observability for the
    /// benchmark and tests; the whole point of the cache is to keep this low on rebuilds.
    reads: AtomicU64,
}

impl Cas {
    /// Open (creating if necessary) a store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        let objects = root.join("objects");
        fs::create_dir_all(&objects)?;
        let cache_path = root.join("digest-cache");
        let digest_cache = Mutex::new(load_digest_cache(&cache_path));
        Ok(Cas {
            objects,
            cache_path,
            digest_cache,
            reads: AtomicU64::new(0),
        })
    }

    /// Store `bytes`, returning their content address. Idempotent: storing the same
    /// content twice writes once. Writes are atomic (temp file + rename) so a
    /// concurrent reader never observes a partial blob.
    pub fn put(&self, bytes: &[u8]) -> io::Result<Digest> {
        let digest = Digest::of(bytes);
        let path = self.blob_path(&digest);
        if path.exists() {
            return Ok(digest);
        }
        let shard = path.parent().expect("blob path always has a shard parent");
        fs::create_dir_all(shard)?;

        let nonce = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = shard.join(format!(".tmp.{}.{}", std::process::id(), nonce));
        fs::write(&tmp, bytes)?;
        match fs::rename(&tmp, &path) {
            Ok(()) => Ok(digest),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                // A racing `put` of identical content may have created it first;
                // that is success, not failure (content-addressing makes them equal).
                if path.exists() {
                    Ok(digest)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Ingest a file *from disk* into the store, returning its content address — the
    /// cached form of `cas.put(&fs::read(path)?)`.
    ///
    /// Reading and SHA-256-ing a file scales with its size and, across a whole input
    /// tree, dominates analysis on a file-heavy repo (vendored deps = thousands of
    /// files). So we cache `(path, mtime, size) → digest`: if the file's mtime and size
    /// match a prior ingest *and* that blob is still in the store, we return the cached
    /// digest after only a `stat` — never touching the file body. On any mismatch (or a
    /// missing blob — the cache self-heals against GC/corruption) we fall back to the
    /// full read+hash and refresh the entry.
    ///
    /// This is content-*blind*, the same trade-off cargo's own fingerprint makes: a
    /// content change that preserves both mtime and size would be missed. Real edits
    /// always move mtime, and the cache is local + cheaply rebuilt, but callers needing
    /// absolute certainty should use [`put`](Cas::put) on bytes they have read themselves.
    pub fn ingest_file(&self, path: &Path) -> io::Result<Digest> {
        let meta = fs::metadata(path)?;
        let identity = file_identity(&meta);

        // Fast path: a matching identity whose blob is still present.
        if let Some((mtime, size)) = identity {
            let cached = self.digest_cache.lock().unwrap().entries.get(path).copied();
            if let Some(e) = cached {
                if e.mtime_nanos == mtime && e.size == size && self.has(&e.digest) {
                    return Ok(e.digest);
                }
            }
        }

        // Slow path: read + hash + store, then record the identity for next time.
        self.reads.fetch_add(1, Ordering::Relaxed);
        let bytes = fs::read(path)?;
        let digest = self.put(&bytes)?;
        if let Some((mtime, size)) = identity {
            let mut cache = self.digest_cache.lock().unwrap();
            cache.entries.insert(
                path.to_path_buf(),
                CacheEntry {
                    mtime_nanos: mtime,
                    size,
                    digest,
                },
            );
            cache.dirty = true;
        }
        Ok(digest)
    }

    /// Number of files actually read+hashed via [`ingest_file`](Cas::ingest_file) — i.e.
    /// digest-cache misses. A no-op rebuild should report ~0.
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// Persist the digest cache atomically (temp + rename). A no-op if nothing changed
    /// since the last flush. Best-effort on drop; callers may invoke it explicitly.
    pub fn flush(&self) -> io::Result<()> {
        let mut cache = self.digest_cache.lock().unwrap();
        if !cache.dirty {
            return Ok(());
        }
        let mut buf = String::new();
        for (path, e) in &cache.entries {
            // Source paths never contain newlines; a non-UTF-8 path that round-trips
            // lossily just misses the cache next time (safe — a re-hash, not an error).
            let _ = writeln!(
                buf,
                "{}\t{}\t{}\t{}",
                e.mtime_nanos,
                e.size,
                e.digest.to_hex(),
                path.display()
            );
        }
        let nonce = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = self.cache_path.with_file_name(format!(
            ".digest-cache.tmp.{}.{}",
            std::process::id(),
            nonce
        ));
        fs::write(&tmp, &buf)?;
        fs::rename(&tmp, &self.cache_path)?;
        cache.dirty = false;
        Ok(())
    }

    /// Fetch the bytes for `digest`, or `None` if absent.
    pub fn get(&self, digest: &Digest) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.blob_path(digest)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Whether `digest` is present in the store.
    pub fn has(&self, digest: &Digest) -> bool {
        self.blob_path(digest).exists()
    }

    /// Place the blob for `digest` at `dest`, creating parent directories. O(1) via a
    /// CoW clone (macOS/APFS) or a hardlink (elsewhere), falling back to a copy across
    /// filesystems. Errors if the blob is absent. See the module docs for how this
    /// keeps a materialized input from corrupting the store.
    pub fn link_into(&self, digest: &Digest, dest: &Path) -> io::Result<LinkKind> {
        let src = self.blob_path(digest);
        if !src.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("CAS blob {digest} not present"),
            ));
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        place_blob(&src, dest)
    }

    /// The on-disk path of a blob: `objects/<first 2 hex>/<remaining 62 hex>`.
    /// Prefix sharding keeps any one directory from growing without bound. Private:
    /// the layout is an implementation detail callers must not depend on.
    fn blob_path(&self, digest: &Digest) -> PathBuf {
        let hex = digest.to_hex();
        self.objects.join(&hex[..2]).join(&hex[2..])
    }
}

impl Drop for Cas {
    /// Persist the digest cache on the way out (best-effort; it is rebuildable, so a
    /// flush failure or a crash just costs a re-hash on the next build).
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// `(mtime-nanoseconds, size)` identity for the digest cache, or `None` if the platform
/// can't give a stable mtime (then we always read+hash — safe, just not cached).
fn file_identity(meta: &fs::Metadata) -> Option<(u128, u64)> {
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((mtime, meta.len()))
}

/// Load the digest cache from disk. A missing/corrupt file, or any unparseable line, is
/// tolerated: the entry is simply dropped (worst case, a re-hash). Format per line:
/// `<mtime_nanos>\t<size>\t<digest_hex>\t<path>`.
fn load_digest_cache(path: &Path) -> DigestCache {
    let mut entries = HashMap::new();
    if let Ok(text) = fs::read_to_string(path) {
        for line in text.lines() {
            let mut parts = line.splitn(4, '\t');
            let (Some(mt), Some(sz), Some(dg), Some(p)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let (Ok(mtime_nanos), Ok(size), Ok(digest)) =
                (mt.parse::<u128>(), sz.parse::<u64>(), Digest::from_hex(dg))
            else {
                continue;
            };
            entries.insert(
                PathBuf::from(p),
                CacheEntry {
                    mtime_nanos,
                    size,
                    digest,
                },
            );
        }
    }
    DigestCache {
        entries,
        dirty: false,
    }
}

/// Place a blob at `dest` using the cheapest store-safe mechanism for the platform.
///
/// macOS/APFS: `clonefile` (copy-on-write, distinct inode) then mark read-only. A
/// write to the input COWs, so the store blob is never mutated.
#[cfg(target_os = "macos")]
fn place_blob(src: &Path, dest: &Path) -> io::Result<LinkKind> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let to_cstring = |p: &Path| {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
    };
    let src_c = to_cstring(src)?;
    let dst_c = to_cstring(dest)?;

    // SAFETY: both arguments are valid NUL-terminated paths; flags 0 is the default.
    let rc = unsafe { libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
    if rc == 0 {
        set_read_only(dest)?;
        return Ok(LinkKind::Cloned);
    }

    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // Not a CoW volume, or a different filesystem: fall back to a copy.
        Some(libc::ENOTSUP) | Some(libc::EXDEV) | Some(libc::ENOSYS) => {
            fs::copy(src, dest)?;
            set_read_only(dest)?;
            Ok(LinkKind::Copied)
        }
        _ => Err(err),
    }
}

/// Mark a freshly-placed input read-only. Safe on a clone/copy (distinct inode); it
/// does not affect the store blob.
#[cfg(target_os = "macos")]
fn set_read_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o444))
}

/// Linux/other: hardlink (shared inode), copy across filesystems. Strict read-only
/// enforcement is the future kernel bind-mount path, so the shared inode is left as-is.
#[cfg(not(target_os = "macos"))]
fn place_blob(src: &Path, dest: &Path) -> io::Result<LinkKind> {
    match fs::hard_link(src, dest) {
        Ok(()) => Ok(LinkKind::Hardlinked),
        Err(e) if e.raw_os_error() == Some(EXDEV) => {
            fs::copy(src, dest)?;
            Ok(LinkKind::Copied)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_round_trip_and_addressing() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();

        let digest = cas.put(b"hello world").unwrap();
        assert_eq!(digest, Digest::of(b"hello world"));
        assert!(cas.has(&digest));
        assert_eq!(
            cas.get(&digest).unwrap().as_deref(),
            Some(&b"hello world"[..])
        );
    }

    #[test]
    fn get_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let absent = Digest::of(b"never stored");
        assert!(!cas.has(&absent));
        assert_eq!(cas.get(&absent).unwrap(), None);
    }

    #[test]
    fn put_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let d1 = cas.put(b"dup").unwrap();
        let d2 = cas.put(b"dup").unwrap();
        assert_eq!(d1, d2);
    }

    #[test]
    fn link_into_places_content() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let digest = cas.put(b"materialize me").unwrap();

        let dest = dir.path().join("sandbox/nested/file.txt");
        let kind = cas.link_into(&digest, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"materialize me");

        use std::os::unix::fs::MetadataExt;
        let src_ino = fs::metadata(cas.blob_path(&digest)).unwrap().ino();
        let dest_ino = fs::metadata(&dest).unwrap().ino();

        #[cfg(target_os = "macos")]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(kind, LinkKind::Cloned);
            assert_ne!(src_ino, dest_ino, "a CoW clone is a distinct inode");
            let mode = fs::metadata(&dest).unwrap().permissions().mode();
            assert_eq!(mode & 0o222, 0, "materialized input is read-only");
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(kind, LinkKind::Hardlinked);
            assert_eq!(src_ino, dest_ino, "hardlink shares the inode");
        }
    }

    /// The store must survive a misbehaving action that writes to its inputs. On
    /// APFS, copy-on-write guarantees this even if the read-only bit is cleared.
    #[cfg(target_os = "macos")]
    #[test]
    fn writing_through_a_materialized_input_cannot_corrupt_the_store() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let digest = cas.put(b"original").unwrap();

        let dest = dir.path().join("sandbox/in.txt");
        cas.link_into(&digest, &dest).unwrap();

        // Clear the read-only bit and overwrite — as a buggy build action might.
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&dest, b"corrupted!!!").unwrap();

        // The clone diverged; the store blob is intact.
        assert_eq!(fs::read(&dest).unwrap(), b"corrupted!!!");
        assert_eq!(cas.get(&digest).unwrap().as_deref(), Some(&b"original"[..]));
    }

    #[test]
    fn link_into_missing_blob_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let absent = Digest::of(b"absent");
        let err = cas.link_into(&absent, &dir.path().join("x")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn ingest_file_addresses_like_put_and_caches_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let f = dir.path().join("src.rs");
        fs::write(&f, b"fn main() {}").unwrap();

        let d1 = cas.ingest_file(&f).unwrap();
        assert_eq!(
            d1,
            Digest::of(b"fn main() {}"),
            "ingest addresses by content like put"
        );
        assert_eq!(cas.reads(), 1, "first ingest reads the file");

        // Unchanged file (same mtime + size): a cache hit, no second read.
        let d2 = cas.ingest_file(&f).unwrap();
        assert_eq!(d2, d1);
        assert_eq!(cas.reads(), 1, "an unchanged file is not re-read/re-hashed");
    }

    #[test]
    fn ingest_file_rehashes_when_content_changes() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let f = dir.path().join("src.rs");
        fs::write(&f, b"v1").unwrap();
        let d1 = cas.ingest_file(&f).unwrap();

        // A real edit moves the mtime (and here the size) → cache miss → fresh digest.
        // Sleep a hair so the mtime is observably different even on coarse clocks.
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&f, b"v2 longer").unwrap();
        let d2 = cas.ingest_file(&f).unwrap();
        assert_ne!(d1, d2, "an edited file must produce a new digest");
        assert_eq!(d2, Digest::of(b"v2 longer"));
        assert_eq!(cas.reads(), 2, "the edit forces a re-read");
    }

    #[test]
    fn digest_cache_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("src.rs");
        fs::write(&f, b"persisted").unwrap();

        let d = {
            let cas = Cas::open(dir.path()).unwrap();
            let d = cas.ingest_file(&f).unwrap();
            cas.flush().unwrap();
            d
        };

        // A fresh Cas over the same root loads the cache: the unchanged file is a hit.
        let cas2 = Cas::open(dir.path()).unwrap();
        let d2 = cas2.ingest_file(&f).unwrap();
        assert_eq!(d, d2);
        assert_eq!(
            cas2.reads(),
            0,
            "a persisted entry means no read after reopen"
        );
    }
}
