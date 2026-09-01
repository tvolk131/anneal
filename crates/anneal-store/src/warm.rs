//! The warm-tree commit record (`docs/proposals/anneal-store.md` §3.2).
//!
//! A [`WarmManifest`] is one small file under `local/warm-meta/<key>/inputs`
//! whose **presence is a transaction commit** and whose **contents are the
//! reuse baseline**. It records, for every declared input of the warm working
//! tree: the content digest anneal placed, and the `(mtime, size)` the file
//! carried at commit. It lives *beside* the warm tree, never inside it — the
//! record vouches for the tree, so it must be unreachable from the process
//! running inside the tree.
//!
//! Two roles, one file:
//!
//! - **Commit record** — the executor's warm transaction does
//!   BEGIN (delete the manifest) → sync inputs → run the tool → COMMIT (write
//!   the manifest atomically, on success only). Absence means "unproven", and
//!   the next run cold-populates rather than trusting a half-synced tree.
//! - **Drift baseline** — on reuse, an input whose digest is unchanged is
//!   still stat-checked against the recorded `(mtime, size)`; a mismatch means
//!   something touched the file since the commit (power loss tearing a just-
//!   placed input, an external edit, a `LoudBestEffort` escape), and it is
//!   re-placed — with a fresh mtime, so the native tool rebuilds exactly what
//!   depended on it. The check self-heals in the safe direction.
//!
//! The `owner` line is informational (for `doctor` and puzzled humans); owner
//! identity is enforced by the state key itself, not by this file.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anneal_core::Digest;

use crate::Recovered;

const MANIFEST_HEADER: &str = "anneal-warm v2";

/// One declared input as recorded at commit: content digest + the stat facts
/// drift-checking needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarmEntry {
    pub digest: Digest,
    /// mtime in nanoseconds since the epoch, as stat'd at commit.
    pub mtime_nanos: u128,
    pub size: u64,
}

impl WarmEntry {
    /// Whether the file at `path` still matches this entry's recorded stat.
    /// Any mismatch — including an unreadable or missing file — is `false`:
    /// the caller then re-places the input, which is always safe (never
    /// unsound) because a re-placed file gets a fresh mtime.
    pub fn matches_file(&self, path: &Path) -> bool {
        match fs::metadata(path) {
            Ok(meta) => {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos());
                mtime == Some(self.mtime_nanos) && meta.len() == self.size
            }
            Err(_) => false,
        }
    }
}

/// The committed baseline of one warm working tree: the owner (informational)
/// and the declared-input entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmManifest {
    owner: String,
    entries: BTreeMap<PathBuf, WarmEntry>,
}

impl WarmManifest {
    pub fn new(owner: impl Into<String>, entries: BTreeMap<PathBuf, WarmEntry>) -> Self {
        WarmManifest {
            owner: owner.into(),
            entries,
        }
    }

    /// The action (and through it, target) that committed this baseline.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn entry(&self, path: &Path) -> Option<&WarmEntry> {
        self.entries.get(path)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&PathBuf, &WarmEntry)> {
        self.entries.iter()
    }

    /// Record the manifest for a `desired` input set by stat'ing the tree at
    /// `cwd` — called at COMMIT, after a successful run. A file that cannot be
    /// stat'd records zeroed stat facts, which simply forces a re-place on the
    /// next reuse (the safe direction).
    pub fn record(
        owner: &str,
        cwd: &Path,
        desired: &BTreeMap<PathBuf, Digest>,
    ) -> io::Result<WarmManifest> {
        let mut entries = BTreeMap::new();
        for (rel, digest) in desired {
            let (mtime_nanos, size) = match fs::metadata(cwd.join(rel)) {
                Ok(meta) => (
                    meta.modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                        .unwrap_or(0),
                    meta.len(),
                ),
                Err(_) => (0, 0),
            };
            entries.insert(
                rel.clone(),
                WarmEntry {
                    digest: *digest,
                    mtime_nanos,
                    size,
                },
            );
        }
        Ok(WarmManifest {
            owner: owner.to_owned(),
            entries,
        })
    }

    /// Write the manifest atomically (temp + rename), so its presence implies
    /// a complete file — the property that lets it double as the commit record.
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
        fs::write(&tmp, self.serialize())?;
        match fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    fn serialize(&self) -> String {
        let mut out = String::new();
        out.push_str(MANIFEST_HEADER);
        out.push('\n');
        out.push_str("owner ");
        out.push_str(&self.owner);
        out.push('\n');
        for (path, entry) in &self.entries {
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\n",
                entry.digest.to_hex(),
                entry.mtime_nanos,
                entry.size,
                path.to_string_lossy()
            ));
        }
        out
    }
}

/// Load a warm manifest through the tolerant path (§3.2 of the proposal):
///
/// - absent or header-corrupt → [`Recovered::Absent`] — no clean baseline, the
///   caller cold-populates, never a partial sync;
/// - entries with unparseable lines → [`Recovered::Degraded`] with the good
///   prefix: a *dropped* entry simply looks "not in the baseline" next reuse,
///   so its file is re-placed — the failure direction that costs work, never
///   correctness.
pub(crate) fn load_warm_manifest(path: &Path) -> io::Result<Recovered<WarmManifest>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Recovered::Absent),
        Err(e) => return Err(e),
    };
    let mut lines = text.lines();
    if lines.next() != Some(MANIFEST_HEADER) {
        // A torn or foreign manifest is "unproven", not an error: cold-populate.
        return Ok(Recovered::Absent);
    }
    let Some(owner) = lines.next().and_then(|l| l.strip_prefix("owner ")) else {
        return Ok(Recovered::Absent);
    };
    let owner = owner.trim_end_matches('\r').to_owned();

    let mut entries = BTreeMap::new();
    let mut dropped = 0usize;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(4, '\t');
        let (Some(hex), Some(mtime), Some(size), Some(path)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            dropped += 1;
            continue;
        };
        match (
            Digest::from_hex(hex),
            mtime.parse::<u128>(),
            size.parse::<u64>(),
        ) {
            (Ok(digest), Ok(mtime_nanos), Ok(size)) => {
                entries.insert(
                    PathBuf::from(path),
                    WarmEntry {
                        digest,
                        mtime_nanos,
                        size,
                    },
                );
            }
            _ => dropped += 1,
        }
    }

    if dropped > 0 {
        Ok(Recovered::Degraded {
            value: WarmManifest { owner, entries },
            note: format!("{dropped} unparseable warm-manifest line(s) dropped"),
        })
    } else {
        Ok(Recovered::Clean(WarmManifest { owner, entries }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired(pairs: &[(&str, Digest)]) -> BTreeMap<PathBuf, Digest> {
        pairs.iter().map(|(p, d)| (PathBuf::from(p), *d)).collect()
    }

    #[test]
    fn manifest_round_trips_with_owner_and_stat_facts() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        fs::create_dir_all(cwd.join("src")).unwrap();
        fs::write(cwd.join("Cargo.toml"), b"[package]").unwrap();
        fs::write(cwd.join("src/lib.rs"), b"fn f() {}").unwrap();

        let m = WarmManifest::record(
            "cargo //pkg:ws",
            &cwd,
            &desired(&[
                ("Cargo.toml", Digest::of(b"[package]")),
                ("src/lib.rs", Digest::of(b"fn f() {}")),
            ]),
        )
        .unwrap();
        let path = dir.path().join("warm-meta/abc/inputs");
        m.save_atomic(&path).unwrap();

        match load_warm_manifest(&path).unwrap() {
            Recovered::Clean(loaded) => {
                assert_eq!(loaded.owner(), "cargo //pkg:ws");
                assert_eq!(
                    loaded.entry(&PathBuf::from("Cargo.toml")),
                    m.entry(&PathBuf::from("Cargo.toml"))
                );
                // The recorded stat facts match the live files.
                assert!(m
                    .entry(&PathBuf::from("src/lib.rs"))
                    .unwrap()
                    .matches_file(&cwd.join("src/lib.rs")));
            }
            other => panic!("expected Clean, got {other:?}"),
        }
        // Absent manifest → Absent (the "no clean baseline" signal).
        assert!(matches!(
            load_warm_manifest(&dir.path().join("warm-meta/none")).unwrap(),
            Recovered::Absent
        ));
    }

    #[test]
    fn torn_tail_degrades_to_the_good_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inputs");
        let m = WarmManifest::new(
            "owner",
            desired(&[("a", Digest::of(b"a")), ("b", Digest::of(b"b"))])
                .into_iter()
                .map(|(p, d)| {
                    (
                        p,
                        WarmEntry {
                            digest: d,
                            mtime_nanos: 1,
                            size: 1,
                        },
                    )
                })
                .collect(),
        );
        m.save_atomic(&path).unwrap();
        // Tear the tail: truncate mid-entry.
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, &text[..text.len() - 5]).unwrap();

        match load_warm_manifest(&path).unwrap() {
            Recovered::Degraded { value, note } => {
                assert!(note.contains("1 unparseable"));
                // The good prefix survives; the dropped entry re-places next reuse.
                assert!(value.entry(&PathBuf::from("a")).is_some());
                assert!(value.entry(&PathBuf::from("b")).is_none());
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    #[test]
    fn foreign_or_headerless_manifest_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inputs");
        fs::write(&path, "not a manifest at all\n").unwrap();
        assert!(matches!(
            load_warm_manifest(&path).unwrap(),
            Recovered::Absent
        ));
    }

    #[test]
    fn drifted_file_fails_the_stat_check() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.rs");
        fs::write(&f, b"v1").unwrap();
        let m = WarmManifest::record("o", dir.path(), &desired(&[("x.rs", Digest::of(b"v1"))]))
            .unwrap();
        assert!(m.entry(&PathBuf::from("x.rs")).unwrap().matches_file(&f));

        // Same content is fine after re-write? No: a rewrite moves the mtime,
        // and drift-checking is content-blind by design — re-place is the safe
        // response and the next commit re-records the fresh stat.
        fs::write(&f, b"v2").unwrap();
        assert!(!m.entry(&PathBuf::from("x.rs")).unwrap().matches_file(&f));
        // Missing file: also a mismatch (re-place).
        fs::remove_file(&f).unwrap();
        assert!(!m.entry(&PathBuf::from("x.rs")).unwrap().matches_file(&f));
    }
}
