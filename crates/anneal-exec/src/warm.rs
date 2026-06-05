//! Warm-sandbox input sync (`docs/sandboxing.md` §5).
//!
//! A snapshot *owner* can keep its working tree in place across builds instead of
//! re-materializing into a fresh sandbox every time. On reuse we reconcile only the
//! **declared inputs** that actually changed, so the inner tool (cargo, …) sees stable
//! mtimes on everything else and does minimal incremental work.
//!
//! This module is the reconciliation engine — deliberately decoupled from the `Action`
//! model: the caller passes a plain `path -> digest` map of the desired inputs and the
//! previous [`InputManifest`]. The two correctness rules from §5.5 live here:
//!
//! * **Unchanged files are left untouched** — preserving their mtime so the inner tool
//!   skips them.
//! * **Added/changed files are placed with a distinct inode and a fresh mtime** — never a
//!   shared-inode hardlink/clone carrying a (possibly stale) CAS-blob mtime. The mtime
//!   experiment proved cargo's freshness check is mtime-based and content-blind, so a
//!   stale mtime on changed content is *silently missed* → a correctness bug. A plain
//!   `fs::write` of a fresh file sets mtime to now, which is exactly what we need.
//!
//! Only declared input paths are touched — never `target/` (the warm snapshot).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anneal_cas::Cas;
use anneal_core::Digest;

/// What is materialized in a warm working tree: declared-input path → content digest.
/// Persisted in `warm-meta/<key>/inputs` as the diff baseline for the next reuse.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct InputManifest {
    entries: BTreeMap<PathBuf, Digest>,
}

impl InputManifest {
    pub(crate) fn new(entries: BTreeMap<PathBuf, Digest>) -> Self {
        InputManifest { entries }
    }

    /// Read a manifest from `path`. `Ok(None)` if it is absent — i.e. no clean baseline,
    /// so the caller must fall back to a cold/restored population, never a partial sync.
    pub(crate) fn load(path: &Path) -> io::Result<Option<Self>> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(Some(parse(&text)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Write the manifest atomically (temp + rename), so its presence implies a complete
    /// file — the property that lets it double as the commit record (§5.4).
    pub(crate) fn save_atomic(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_file_name(format!(
            "{}.tmp.{}",
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("inputs"),
            std::process::id()
        ));
        fs::write(&tmp, serialize(self))?;
        match fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

/// Per-sync outcome counts (observability and tests).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SyncStats {
    pub left: usize,
    pub replaced: usize,
    pub added: usize,
    pub removed: usize,
}

/// Reconcile the warm working tree at `cwd` from the `old` manifest to `desired`
/// (`path -> digest`). Returns the per-category counts. Touches only the paths in `old`
/// and `desired` — never `target/` or anything else in the tree.
pub(crate) fn sync(
    cas: &Cas,
    cwd: &Path,
    old: &InputManifest,
    desired: &BTreeMap<PathBuf, Digest>,
    writable: &BTreeSet<PathBuf>,
) -> io::Result<SyncStats> {
    let mut stats = SyncStats::default();

    // Added / changed / unchanged.
    for (rel, digest) in desired {
        let is_writable = writable.contains(rel);
        match old.entries.get(rel) {
            Some(prev) if prev == digest && !is_writable => stats.left += 1,
            Some(_) => {
                place_fresh(cas, &cwd.join(rel), digest, is_writable)?;
                stats.replaced += 1;
            }
            None => {
                place_fresh(cas, &cwd.join(rel), digest, is_writable)?;
                stats.added += 1;
            }
        }
    }

    // Removed: present last time, not declared now. Leaving a stale source file behind
    // is a phantom compile, so this is correctness, not tidiness.
    for rel in old.entries.keys() {
        if !desired.contains_key(rel) {
            remove(&cwd.join(rel))?;
            stats.removed += 1;
        }
    }

    Ok(stats)
}

/// Place `digest`'s content at `dest` as a **fresh** file: a distinct inode (a copy,
/// never a shared-inode hardlink/clone) with mtime = now. Any existing file is removed
/// first — it may be read-only and/or a shared-inode clone, and a plain overwrite would
/// either fail or carry a stale mtime. `fs::write` of a new file stamps mtime = now,
/// which is the freshness cargo's mtime-based check requires (§5.5).
fn place_fresh(cas: &Cas, dest: &Path, digest: &Digest, writable: bool) -> io::Result<()> {
    let bytes = cas.get(digest)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("CAS blob {digest} not present"),
        )
    })?;
    remove(dest)?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(dest, &bytes)?;
    set_mode(dest, writable)
}

/// Remove a file if present (idempotent). Works on a read-only file — removal needs write
/// permission on the parent directory, not on the file.
fn remove(dest: &Path) -> io::Result<()> {
    match fs::remove_file(dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, writable: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if writable { 0o644 } else { 0o444 }),
    )
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _writable: bool) -> io::Result<()> {
    Ok(())
}

fn serialize(m: &InputManifest) -> String {
    let mut out = String::new();
    for (path, digest) in &m.entries {
        out.push_str(&digest.to_hex());
        out.push('\t');
        out.push_str(&path.to_string_lossy());
        out.push('\n');
    }
    out
}

fn parse(text: &str) -> io::Result<InputManifest> {
    let mut entries = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let (hex, path) = line.split_once('\t').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("manifest line {}: missing tab", i + 1),
            )
        })?;
        let digest = Digest::from_hex(hex).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("manifest line {}: {e}", i + 1),
            )
        })?;
        entries.insert(PathBuf::from(path), digest);
    }
    Ok(InputManifest { entries })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime};

    fn setup() -> (tempfile::TempDir, Cas, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Cas::open(tmp.path().join("cas")).unwrap();
        let cwd = tmp.path().join("work");
        fs::create_dir_all(&cwd).unwrap();
        (tmp, cas, cwd)
    }

    fn manifest(pairs: &[(&str, Digest)]) -> InputManifest {
        InputManifest::new(pairs.iter().map(|(p, d)| (PathBuf::from(p), *d)).collect())
    }
    fn desired(pairs: &[(&str, Digest)]) -> BTreeMap<PathBuf, Digest> {
        pairs.iter().map(|(p, d)| (PathBuf::from(p), *d)).collect()
    }
    fn writable(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }
    fn mtime(p: &Path) -> SystemTime {
        fs::metadata(p).unwrap().modified().unwrap()
    }
    fn inode(p: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(p).unwrap().ino()
    }

    #[test]
    fn unchanged_file_is_left_untouched() {
        // An unchanged file must keep its identity (same inode) and mtime, so the inner
        // tool's fingerprint skips it.
        let (_t, cas, cwd) = setup();
        let d = cas.put(b"hello").unwrap();
        place_fresh(&cas, &cwd.join("src/lib.rs"), &d, false).unwrap();
        let before_ino = inode(&cwd.join("src/lib.rs"));
        let before_mtime = mtime(&cwd.join("src/lib.rs"));

        let stats = sync(
            &cas,
            &cwd,
            &manifest(&[("src/lib.rs", d)]),
            &desired(&[("src/lib.rs", d)]),
            &writable(&[]),
        )
        .unwrap();

        assert_eq!(
            stats,
            SyncStats {
                left: 1,
                ..Default::default()
            }
        );
        assert_eq!(
            inode(&cwd.join("src/lib.rs")),
            before_ino,
            "unchanged file must not be rewritten"
        );
        assert_eq!(
            mtime(&cwd.join("src/lib.rs")),
            before_mtime,
            "unchanged file must keep its mtime"
        );
    }

    #[test]
    fn changed_file_gets_new_content_and_a_fresh_mtime() {
        // The mtime-hazard fix: even if the on-disk file carries a STALE mtime (as a
        // hardlink/clone of an old CAS blob would), after sync the changed file must have
        // current content AND a fresh mtime — or cargo would silently skip the change.
        let (_t, cas, cwd) = setup();
        let old = cas.put(b"const V: u32 = 1;").unwrap();
        let new = cas.put(b"const V: u32 = 2;").unwrap();
        let p = cwd.join("src/lib.rs");

        // Stage an old file and backdate its mtime by an hour (simulate the stale blob).
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"const V: u32 = 1;").unwrap();
        let stale = SystemTime::now() - Duration::from_secs(3600);
        OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(stale)
            .unwrap();
        assert!(
            mtime(&p) < SystemTime::now() - Duration::from_secs(1800),
            "precondition: mtime is stale"
        );

        let before = SystemTime::now();
        let stats = sync(
            &cas,
            &cwd,
            &manifest(&[("src/lib.rs", old)]),
            &desired(&[("src/lib.rs", new)]),
            &writable(&[]),
        )
        .unwrap();

        assert_eq!(
            stats,
            SyncStats {
                replaced: 1,
                ..Default::default()
            }
        );
        assert_eq!(
            fs::read(&p).unwrap(),
            b"const V: u32 = 2;",
            "content must be the new blob"
        );
        assert!(
            mtime(&p) >= before,
            "changed file must get a fresh mtime, not the stale one"
        );
        // Read-only, like a materialized input.
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o444
        );
    }

    #[test]
    fn added_file_is_created_and_removed_file_is_unlinked() {
        let (_t, cas, cwd) = setup();
        let keep = cas.put(b"keep").unwrap();
        let gone = cas.put(b"gone").unwrap();
        let fresh = cas.put(b"fresh").unwrap();
        place_fresh(&cas, &cwd.join("keep.rs"), &keep, false).unwrap();
        place_fresh(&cas, &cwd.join("gone.rs"), &gone, false).unwrap();

        let stats = sync(
            &cas,
            &cwd,
            &manifest(&[("keep.rs", keep), ("gone.rs", gone)]),
            &desired(&[("keep.rs", keep), ("new.rs", fresh)]),
            &writable(&[]),
        )
        .unwrap();

        assert_eq!(
            stats,
            SyncStats {
                left: 1,
                added: 1,
                removed: 1,
                ..Default::default()
            }
        );
        assert!(cwd.join("new.rs").exists(), "added file must be created");
        assert!(
            !cwd.join("gone.rs").exists(),
            "removed file must be unlinked"
        );
        assert_eq!(fs::read(cwd.join("new.rs")).unwrap(), b"fresh");
    }

    #[test]
    fn replacing_a_read_only_file_succeeds() {
        // place_fresh leaves files 0444; the next sync must still be able to replace them.
        let (_t, cas, cwd) = setup();
        let a = cas.put(b"a").unwrap();
        let b = cas.put(b"b").unwrap();
        place_fresh(&cas, &cwd.join("x.rs"), &a, false).unwrap();
        assert_eq!(
            fs::metadata(cwd.join("x.rs")).unwrap().permissions().mode() & 0o777,
            0o444
        );

        sync(
            &cas,
            &cwd,
            &manifest(&[("x.rs", a)]),
            &desired(&[("x.rs", b)]),
            &writable(&[]),
        )
        .unwrap();
        assert_eq!(fs::read(cwd.join("x.rs")).unwrap(), b"b");
    }

    #[test]
    fn writable_input_is_refreshed_even_when_digest_is_unchanged() {
        // A mutable input may have been edited by the prior warm run, so the next reuse
        // cannot trust the on-disk bytes just because the desired digest is unchanged.
        let (_t, cas, cwd) = setup();
        let lock = cas.put(b"lockfile").unwrap();
        let p = cwd.join("pnpm-lock.yaml");
        place_fresh(&cas, &p, &lock, true).unwrap();
        fs::write(&p, b"mutated by tool").unwrap();

        let stats = sync(
            &cas,
            &cwd,
            &manifest(&[("pnpm-lock.yaml", lock)]),
            &desired(&[("pnpm-lock.yaml", lock)]),
            &writable(&["pnpm-lock.yaml"]),
        )
        .unwrap();

        assert_eq!(
            stats,
            SyncStats {
                replaced: 1,
                ..Default::default()
            }
        );
        assert_eq!(fs::read(&p).unwrap(), b"lockfile");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn manifest_round_trips_through_disk() {
        let (_t, _cas, cwd) = setup();
        let m = manifest(&[
            ("Cargo.toml", Digest::of(b"1")),
            ("a/src/lib.rs", Digest::of(b"2")),
        ]);
        let path = cwd.join("meta/inputs");
        m.save_atomic(&path).unwrap();
        assert_eq!(InputManifest::load(&path).unwrap(), Some(m));
        // Absent manifest → None (the "no clean baseline" signal).
        assert_eq!(
            InputManifest::load(&cwd.join("meta/missing")).unwrap(),
            None
        );
    }
}
