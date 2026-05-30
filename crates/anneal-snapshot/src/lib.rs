//! `anneal-snapshot` — the snapshot protocol (§8.2).
//!
//! Native tools keep mutable, incremental on-disk state that is not a clean artifact
//! (Cargo's `target/`, pnpm's store). Anneal models these as **snapshots**, governed
//! entirely by the §1.4 invariant: **restoring a snapshot may make a build faster but
//! must never change its semantic output.** A snapshot is an accelerator, never an
//! input that affects results — which is why a snapshot key never enters an action's
//! cache key.
//!
//! This crate is a deep module: the interface is [`SnapshotStore::save`] /
//! [`SnapshotStore::restore`], keyed by a coarse [`Digest`] (e.g. a hash of
//! `(toolchain, lockfile, target_triple, profile)`). It hides how a mutable tree is
//! content-addressed.
//!
//! # Fidelity
//!
//! A snapshot is stored as a CAS-backed **manifest**: each file's content goes into
//! the CAS (so unchanged files dedup across snapshot generations), and the manifest
//! records each entry's path, kind, unix mode, and **nanosecond mtime**. mtime
//! fidelity is essential: Cargo's path-dependency fingerprints are mtime-based, so a
//! lossy (second-granularity) restore could change incremental behavior — exactly
//! what the correctness-neutral invariant forbids.
//!
//! # v1 scope (§8.3)
//!
//! Conservative whole-directory snapshots. One manifest per key (latest wins); no
//! deep tool-internal pruning. Regular files, directories, and symlinks are handled.

use std::fs::{self, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anneal_cas::Cas;
use anneal_core::Digest;

/// A content-addressed store of directory snapshots.
pub struct SnapshotStore {
    /// `<root>/index/<key_hex>` maps a snapshot key to its manifest digest.
    index: PathBuf,
}

impl SnapshotStore {
    /// Open (creating if needed) a snapshot store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let index = root.into().join("index");
        fs::create_dir_all(&index)?;
        Ok(SnapshotStore { index })
    }

    /// Snapshot `dir` under `key`, replacing any prior snapshot for that key. Returns
    /// `false` (a no-op) if `dir` does not exist (nothing was built yet).
    pub fn save(&self, cas: &Cas, key: &Digest, dir: &Path) -> io::Result<bool> {
        if !dir.exists() {
            return Ok(false);
        }
        let mut entries = Vec::new();
        collect(dir, dir, cas, &mut entries)?;
        let manifest_digest = cas.put(&encode(&entries))?;
        write_index(&self.index_path(key), &manifest_digest)?;
        Ok(true)
    }

    /// Restore the snapshot for `key` into `dir` (created if needed). Returns `false`
    /// if no snapshot exists for the key (a cold start — handled gracefully, §8.2).
    pub fn restore(&self, cas: &Cas, key: &Digest, dir: &Path) -> io::Result<bool> {
        let manifest_digest = match read_index(&self.index_path(key))? {
            Some(d) => d,
            None => return Ok(false),
        };
        let manifest = cas
            .get(&manifest_digest)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "snapshot manifest missing"))?;
        let entries = decode(&manifest)?;

        fs::create_dir_all(dir)?;
        // Entries are in parent-first order, so directories exist before their contents.
        for entry in &entries {
            let path = dir.join(&entry.path);
            match &entry.kind {
                EntryKind::Dir => {
                    fs::create_dir_all(&path)?;
                }
                EntryKind::Symlink(target) => {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    std::os::unix::fs::symlink(target, &path)?;
                }
                EntryKind::File(digest) => {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let bytes = cas.get(digest)?.ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "snapshot file content missing")
                    })?;
                    fs::write(&path, &bytes)?;
                    fs::set_permissions(&path, Permissions::from_mode(entry.mode))?;
                    set_mtime(&path, entry.secs, entry.nanos)?;
                }
            }
        }
        Ok(true)
    }

    fn index_path(&self, key: &Digest) -> PathBuf {
        self.index.join(key.to_hex())
    }
}

/// One filesystem entry recorded in a snapshot manifest.
struct Entry {
    path: PathBuf,
    kind: EntryKind,
    mode: u32,
    secs: u64,
    nanos: u32,
}

enum EntryKind {
    Dir,
    File(Digest),
    Symlink(String),
}

/// Walk `current` (under `root`), pushing entries parent-first and storing file
/// contents in the CAS.
fn collect(root: &Path, current: &Path, cas: &Cas, out: &mut Vec<Entry>) -> io::Result<()> {
    let mut children: Vec<_> = fs::read_dir(current)?.collect::<Result<_, _>>()?;
    // Deterministic order.
    children.sort_by_key(|e| e.file_name());

    for child in children {
        let path = child.path();
        let rel = path
            .strip_prefix(root)
            .expect("child is under root")
            .to_path_buf();
        let meta = fs::symlink_metadata(&path)?;
        let mode = meta.permissions().mode();
        let (secs, nanos) = mtime_parts(&meta);
        let file_type = meta.file_type();

        if file_type.is_dir() {
            out.push(Entry {
                path: rel,
                kind: EntryKind::Dir,
                mode,
                secs,
                nanos,
            });
            collect(root, &path, cas, out)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&path)?.to_string_lossy().into_owned();
            out.push(Entry {
                path: rel,
                kind: EntryKind::Symlink(target),
                mode,
                secs,
                nanos,
            });
        } else if file_type.is_file() {
            let digest = cas.put(&fs::read(&path)?)?;
            out.push(Entry {
                path: rel,
                kind: EntryKind::File(digest),
                mode,
                secs,
                nanos,
            });
        }
    }
    Ok(())
}

fn mtime_parts(meta: &fs::Metadata) -> (u64, u32) {
    match meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()) {
        Some(d) => (d.as_secs(), d.subsec_nanos()),
        None => (0, 0),
    }
}

fn set_mtime(path: &Path, secs: u64, nanos: u32) -> io::Result<()> {
    let when = UNIX_EPOCH + Duration::new(secs, nanos);
    OpenOptions::new().write(true).open(path)?.set_modified(when)
}

// --- Manifest encoding (length-prefixed binary) ---

fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut buf = Vec::new();
    put_u64(&mut buf, entries.len() as u64);
    for entry in entries {
        put_u32(&mut buf, entry.mode);
        put_u64(&mut buf, entry.secs);
        put_u32(&mut buf, entry.nanos);
        put_bytes(&mut buf, entry.path.to_string_lossy().as_bytes());
        match &entry.kind {
            EntryKind::Dir => buf.push(0),
            EntryKind::File(digest) => {
                buf.push(1);
                buf.extend_from_slice(digest.as_bytes());
            }
            EntryKind::Symlink(target) => {
                buf.push(2);
                put_bytes(&mut buf, target.as_bytes());
            }
        }
    }
    buf
}

fn decode(bytes: &[u8]) -> io::Result<Vec<Entry>> {
    let mut cur = Cursor { bytes, pos: 0 };
    let count = cur.u64()?;
    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mode = cur.u32()?;
        let secs = cur.u64()?;
        let nanos = cur.u32()?;
        let path = PathBuf::from(String::from_utf8_lossy(cur.bytes()?).into_owned());
        let kind = match cur.u8()? {
            0 => EntryKind::Dir,
            1 => {
                let raw = cur.take(32)?;
                let mut arr = [0u8; 32];
                arr.copy_from_slice(raw);
                EntryKind::File(Digest::from_hex(&hex(&arr)).expect("32 bytes is valid"))
            }
            2 => EntryKind::Symlink(String::from_utf8_lossy(cur.bytes()?).into_owned()),
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad entry tag {other}"),
                ))
            }
        };
        entries.push(Entry {
            path,
            kind,
            mode,
            secs,
            nanos,
        });
    }
    Ok(entries)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_bytes(buf: &mut Vec<u8>, b: &[u8]) {
    put_u64(buf, b.len() as u64);
    buf.extend_from_slice(b);
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.bytes.len());
        match end {
            Some(end) => {
                let slice = &self.bytes[self.pos..end];
                self.pos = end;
                Ok(slice)
            }
            None => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated manifest",
            )),
        }
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> io::Result<&[u8]> {
        let len = self.u64()? as usize;
        self.take(len)
    }
}

fn write_index(path: &Path, manifest_digest: &Digest) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, manifest_digest.to_hex())?;
    fs::rename(&tmp, path)
}

fn read_index(path: &Path) -> io::Result<Option<Digest>> {
    match fs::read_to_string(path) {
        Ok(hex) => Digest::from_hex(hex.trim())
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_restore_preserves_content_mode_and_nanosecond_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path().join("cas")).unwrap();
        let store = SnapshotStore::open(tmp.path().join("snap")).unwrap();

        // Build a source tree with a nested file and a specific mtime.
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();
        fs::write(src.join("nested/b.bin"), [0u8, 1, 2, 3]).unwrap();
        let when = UNIX_EPOCH + Duration::new(1_000_000_000, 123_456_789);
        OpenOptions::new()
            .write(true)
            .open(src.join("a.txt"))
            .unwrap()
            .set_modified(when)
            .unwrap();

        let key = Digest::of(b"toolchain|lock|triple|debug");
        assert!(store.save(&cas, &key, &src).unwrap());

        let dst = tmp.path().join("dst");
        assert!(store.restore(&cas, &key, &dst).unwrap());

        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(dst.join("nested/b.bin")).unwrap(), [0u8, 1, 2, 3]);
        // Nanosecond mtime survives the round trip.
        let restored = fs::metadata(dst.join("a.txt")).unwrap().modified().unwrap();
        assert_eq!(restored, when);
    }

    #[test]
    fn restore_of_unknown_key_is_a_graceful_cold_start() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path().join("cas")).unwrap();
        let store = SnapshotStore::open(tmp.path().join("snap")).unwrap();
        let restored = store
            .restore(&cas, &Digest::of(b"never saved"), &tmp.path().join("out"))
            .unwrap();
        assert!(!restored, "cold start returns false, not an error");
    }

    #[test]
    fn save_is_a_noop_when_dir_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path().join("cas")).unwrap();
        let store = SnapshotStore::open(tmp.path().join("snap")).unwrap();
        let saved = store
            .save(&cas, &Digest::of(b"k"), &tmp.path().join("does-not-exist"))
            .unwrap();
        assert!(!saved);
    }
}
