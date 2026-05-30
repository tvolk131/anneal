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

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anneal_core::Digest;

/// `EXDEV` ("cross-device link") errno — 18 on both Linux and macOS. A hardlink
/// across filesystems fails with this; we fall back to a copy (Spike B finding §4).
const EXDEV: i32 = 18;

/// Disambiguates temp file names within a process so concurrent `put`s don't collide.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How a blob was placed into the destination by [`Cas::link_into`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// Hardlinked — O(1), shares the inode with the CAS blob (the common case).
    Hardlinked,
    /// Copied — destination was on a different filesystem from the store.
    Copied,
}

/// A content-addressed store rooted at a directory.
pub struct Cas {
    /// `<root>/objects` — all blobs live under here, prefix-sharded.
    objects: PathBuf,
}

impl Cas {
    /// Open (creating if necessary) a store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let objects = root.into().join("objects");
        fs::create_dir_all(&objects)?;
        Ok(Cas { objects })
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

    /// Place the blob for `digest` at `dest`, creating parent directories. Hardlinks
    /// from the store (O(1), shared inode); falls back to a copy if `dest` is on a
    /// different filesystem. Errors if the blob is absent.
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
        match fs::hard_link(&src, dest) {
            Ok(()) => Ok(LinkKind::Hardlinked),
            Err(e) if e.raw_os_error() == Some(EXDEV) => {
                fs::copy(&src, dest)?;
                Ok(LinkKind::Copied)
            }
            Err(e) => Err(e),
        }
    }

    /// The on-disk path of a blob: `objects/<first 2 hex>/<remaining 62 hex>`.
    /// Prefix sharding keeps any one directory from growing without bound. Private:
    /// the layout is an implementation detail callers must not depend on.
    fn blob_path(&self, digest: &Digest) -> PathBuf {
        let hex = digest.to_hex();
        self.objects.join(&hex[..2]).join(&hex[2..])
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
        assert_eq!(cas.get(&digest).unwrap().as_deref(), Some(&b"hello world"[..]));
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
    fn link_into_hardlinks_and_shares_inode() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let digest = cas.put(b"materialize me").unwrap();

        let dest = dir.path().join("sandbox/nested/file.txt");
        let kind = cas.link_into(&digest, &dest).unwrap();
        assert_eq!(kind, LinkKind::Hardlinked);
        assert_eq!(fs::read(&dest).unwrap(), b"materialize me");

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let src_ino = fs::metadata(cas.blob_path(&digest)).unwrap().ino();
            let dest_ino = fs::metadata(&dest).unwrap().ino();
            assert_eq!(src_ino, dest_ino, "hardlink must share the inode");
        }
    }

    #[test]
    fn link_into_missing_blob_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let absent = Digest::of(b"absent");
        let err = cas
            .link_into(&absent, &dir.path().join("x"))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
