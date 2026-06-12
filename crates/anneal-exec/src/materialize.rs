//! Materializing provided files into the working tree (`anneal materialize`).
//!
//! Builds put their outputs in the CAS; native tools (`cargo run`,
//! rust-analyzer) read the working tree. [`MaterializeStore`] bridges the two:
//! it writes a target's provided files to their tree destinations and records
//! every write in a manifest (`materialized`, under the store root next to the
//! CAS), so the operation is idempotent, prunable, and reversible.
//!
//! Three invariants govern every mutation:
//!
//! * **Never clobber what anneal didn't write.** A destination that exists but
//!   is absent from the manifest — or whose content drifted from the recorded
//!   digest (the user edited it) — is refused, not overwritten (`force`
//!   overrides).
//! * **Never rewrite identical bytes.** A fresh mtime on unchanged content
//!   triggers spurious native-tool rebuilds — the exact waste materialization
//!   exists to remove. An up-to-date destination is left untouched.
//! * **Removal is digest-guarded.** `clean` (and orphan pruning on re-apply)
//!   deletes a file only while its content still matches the manifest; an
//!   edited file is reported and left in place.
//!
//! Materialized files are written **read-only**: they are generated, and the
//! editable thing is the source they came from. They must also stay invisible
//! to anneal itself — the manifest's path set feeds the analyzer's source-walk
//! exclusion, so a tree copy can never shadow the producing action's declared
//! output (an analysis-time hard error) or perturb snapshot keys. The routed
//! action edge remains the only real input.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use anneal_cas::Cas;
use anneal_core::Digest;

const MANIFEST_HEADER: &str = "anneal-materialized v1";

/// One materialized file: where it was written, which target provided it, and
/// the content digest at write time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedEntry {
    /// Destination, relative to the workspace root.
    pub path: PathBuf,
    /// The providing target's canonical label.
    pub label: String,
    /// The content digest written (and expected back on removal).
    pub digest: Digest,
}

/// The tree's current relationship to a manifest entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeState {
    /// The file's content still matches the manifest digest.
    Intact,
    /// The file exists but its content drifted — the user edited it.
    Edited,
    /// The file is gone.
    Missing,
}

/// A path a mutation deliberately left untouched, and why.
#[derive(Debug, Clone)]
pub struct Refusal {
    pub path: PathBuf,
    pub reason: String,
}

/// What [`MaterializeStore::apply`] did.
#[derive(Debug, Default)]
pub struct ApplyReport {
    pub written: Vec<PathBuf>,
    pub unchanged: Vec<PathBuf>,
    /// Previously materialized by this label, no longer provided — removed.
    pub pruned: Vec<PathBuf>,
    pub refused: Vec<Refusal>,
}

/// What [`MaterializeStore::clean`] did.
#[derive(Debug, Default)]
pub struct CleanReport {
    pub removed: Vec<PathBuf>,
    pub refused: Vec<Refusal>,
}

/// What [`MaterializeStore::check`] found.
#[derive(Debug, Default)]
pub struct CheckReport {
    /// Tree content already matches what the target provides.
    pub fresh: Vec<PathBuf>,
    /// Missing, content-drifted, or orphaned (no longer provided).
    pub stale: Vec<PathBuf>,
}

/// The manifest of materialized files plus the operations over it. All
/// mutations save the manifest (atomically, tmp + rename) before returning.
pub struct MaterializeStore {
    workspace_root: PathBuf,
    manifest_path: PathBuf,
    /// Workspace-relative destination → (providing label, written digest).
    entries: BTreeMap<PathBuf, (String, Digest)>,
}

impl MaterializeStore {
    /// Open the manifest under `store_root` (e.g. `.anneal/`); a missing
    /// manifest is an empty one. Destinations resolve under `workspace_root`.
    pub fn open(
        store_root: impl Into<PathBuf>,
        workspace_root: impl Into<PathBuf>,
    ) -> io::Result<Self> {
        let manifest_path = store_root.into().join("materialized");
        let entries = match fs::read_to_string(&manifest_path) {
            Ok(text) => parse_manifest(&text)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e),
        };
        Ok(MaterializeStore {
            workspace_root: workspace_root.into(),
            manifest_path,
            entries,
        })
    }

    /// Workspace-relative paths of every materialized file — the analyzer's
    /// source-walk exclusion set.
    pub fn paths(&self) -> BTreeSet<PathBuf> {
        self.entries.keys().cloned().collect()
    }

    /// Every manifest entry, ordered by path.
    pub fn entries(&self) -> Vec<MaterializedEntry> {
        self.entries
            .iter()
            .map(|(path, (label, digest))| MaterializedEntry {
                path: path.clone(),
                label: label.clone(),
                digest: *digest,
            })
            .collect()
    }

    /// The tree's current state for a manifest entry.
    pub fn tree_state(&self, entry: &MaterializedEntry) -> io::Result<TreeState> {
        Ok(match tree_digest(&self.workspace_root.join(&entry.path))? {
            None => TreeState::Missing,
            Some(cur) if cur == entry.digest => TreeState::Intact,
            Some(_) => TreeState::Edited,
        })
    }

    /// Write `label`'s provided files into the tree. Per destination: identical
    /// content is left untouched; a file anneal wrote (tree content still
    /// matches the manifest) is rewritten in place; anything else is refused
    /// unless `force`. Entries this label previously materialized but no longer
    /// provides are pruned under the digest guard.
    pub fn apply(
        &mut self,
        label: &str,
        files: &[(PathBuf, Digest)],
        cas: &Cas,
        force: bool,
    ) -> io::Result<ApplyReport> {
        let mut report = ApplyReport::default();
        let desired: BTreeSet<&Path> = files.iter().map(|(p, _)| p.as_path()).collect();

        for (path, digest) in files {
            validate_tree_path(path)?;
            let dest = self.workspace_root.join(path);
            let recorded = self.entries.get(path);

            // A path already claimed by a different target is a collision, not
            // an update — two targets fighting over one destination.
            if let Some((owner, _)) = recorded {
                if owner != label && !force {
                    report.refused.push(Refusal {
                        path: path.clone(),
                        reason: format!(
                            "already materialized by {owner} (use --force to take over)"
                        ),
                    });
                    continue;
                }
            }

            match tree_digest(&dest)? {
                // Right bytes already on disk (ours, or an identical user copy
                // we can safely adopt): record it, touch nothing.
                Some(cur) if cur == *digest => {
                    self.entries
                        .insert(path.clone(), (label.to_owned(), *digest));
                    report.unchanged.push(path.clone());
                }
                Some(cur) => {
                    let ours = recorded.is_some_and(|(_, recorded)| *recorded == cur);
                    if ours || force {
                        write_blob(cas, *digest, &dest)?;
                        self.entries
                            .insert(path.clone(), (label.to_owned(), *digest));
                        report.written.push(path.clone());
                    } else {
                        let reason = if recorded.is_some() {
                            "edited since materialized (use --force to overwrite)"
                        } else {
                            "exists but was not written by anneal (use --force to overwrite)"
                        };
                        report.refused.push(Refusal {
                            path: path.clone(),
                            reason: reason.to_owned(),
                        });
                    }
                }
                None => {
                    write_blob(cas, *digest, &dest)?;
                    self.entries
                        .insert(path.clone(), (label.to_owned(), *digest));
                    report.written.push(path.clone());
                }
            }
        }

        // Prune this label's orphans (a renamed `out`, a removed provider file)
        // so stale generated files don't accumulate invisibly.
        let orphans: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|(path, (owner, _))| owner == label && !desired.contains(path.as_path()))
            .map(|(path, _)| path.clone())
            .collect();
        for path in orphans {
            match self.remove_guarded(&path, force)? {
                Removal::Removed | Removal::AlreadyGone => report.pruned.push(path),
                Removal::Edited => report.refused.push(Refusal {
                    path,
                    reason: "no longer provided, but edited since materialized — left in place \
                             (use --force to remove)"
                        .to_owned(),
                }),
            }
        }

        self.save()?;
        Ok(report)
    }

    /// Compare `label`'s provided files against the tree without writing.
    pub fn check(&self, label: &str, files: &[(PathBuf, Digest)]) -> io::Result<CheckReport> {
        let mut report = CheckReport::default();
        let desired: BTreeSet<&Path> = files.iter().map(|(p, _)| p.as_path()).collect();
        for (path, digest) in files {
            if tree_digest(&self.workspace_root.join(path))? == Some(*digest) {
                report.fresh.push(path.clone());
            } else {
                report.stale.push(path.clone());
            }
        }
        for (path, (owner, _)) in &self.entries {
            if owner == label && !desired.contains(path.as_path()) {
                report.stale.push(path.clone()); // orphan: apply would prune it
            }
        }
        Ok(report)
    }

    /// Remove materialized files — all of them, or only `label`'s.
    pub fn clean(&mut self, label: Option<&str>, force: bool) -> io::Result<CleanReport> {
        let selected: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|(_, (owner, _))| label.is_none_or(|want| owner == want))
            .map(|(path, _)| path.clone())
            .collect();
        let mut report = CleanReport::default();
        for path in selected {
            match self.remove_guarded(&path, force)? {
                Removal::Removed | Removal::AlreadyGone => report.removed.push(path),
                Removal::Edited => report.refused.push(Refusal {
                    path,
                    reason: "edited since materialized — left in place (use --force to remove)"
                        .to_owned(),
                }),
            }
        }
        self.save()?;
        Ok(report)
    }

    /// Remove one entry's file iff its content still matches the manifest (or
    /// `force`). An edited file keeps both the file and its manifest entry, so
    /// it stays visible to `--list` and excluded from source walks.
    fn remove_guarded(&mut self, path: &Path, force: bool) -> io::Result<Removal> {
        let Some((_, digest)) = self.entries.get(path) else {
            return Ok(Removal::AlreadyGone);
        };
        let dest = self.workspace_root.join(path);
        match tree_digest(&dest)? {
            None => {
                self.entries.remove(path);
                Ok(Removal::AlreadyGone)
            }
            Some(cur) if cur == *digest || force => {
                fs::remove_file(&dest)?;
                self.entries.remove(path);
                Ok(Removal::Removed)
            }
            Some(_) => Ok(Removal::Edited),
        }
    }

    /// Persist the manifest: tmp + rename, the action-cache idiom.
    fn save(&self) -> io::Result<()> {
        let mut text = String::from(MANIFEST_HEADER);
        text.push('\n');
        for (path, (label, digest)) in &self.entries {
            text.push_str(&format!(
                "{} {} {}\n",
                digest.to_hex(),
                label,
                path.display()
            ));
        }
        if let Some(parent) = self.manifest_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self
            .manifest_path
            .with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&tmp, text)?;
        match fs::rename(&tmp, &self.manifest_path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

enum Removal {
    Removed,
    AlreadyGone,
    Edited,
}

/// `Some(digest)` of the file's bytes, `None` if absent.
fn tree_digest(path: &Path) -> io::Result<Option<Digest>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(Digest::of(&bytes))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Write the CAS blob to `dest`, read-only (0o444, matching CAS blobs).
fn write_blob(cas: &Cas, digest: Digest, dest: &Path) -> io::Result<()> {
    let bytes = cas.get(&digest)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("blob {digest} is not in the CAS"),
        )
    })?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if dest.exists() {
        fs::remove_file(dest)?; // a prior write is read-only; fs::write can't truncate it
    }
    fs::write(dest, bytes)?;
    fs::set_permissions(dest, fs::Permissions::from_mode(0o444))
}

/// Destinations must stay inside the workspace: relative, normal components only.
fn validate_tree_path(path: &Path) -> io::Result<()> {
    let ok = !path.as_os_str().is_empty()
        && path.components().all(|c| matches!(c, Component::Normal(_)));
    if ok {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "materialize destination `{}` must be a relative path inside the workspace",
                path.display()
            ),
        ))
    }
}

fn parse_manifest(text: &str) -> io::Result<BTreeMap<PathBuf, (String, Digest)>> {
    let invalid = |msg: String| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("materialize manifest: {msg}"),
        )
    };
    let mut lines = text.lines();
    if lines.next() != Some(MANIFEST_HEADER) {
        return Err(invalid(format!("missing `{MANIFEST_HEADER}` header")));
    }
    let mut entries = BTreeMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        // `<digest> <label> <path>` — path last, since labels never contain
        // spaces but paths may.
        let mut parts = line.splitn(3, ' ');
        let (Some(hex), Some(label), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid(format!(
                "entry `{line}` is not `<digest> <label> <path>`"
            )));
        };
        let digest = Digest::from_hex(hex).map_err(|e| invalid(format!("bad digest: {e}")))?;
        entries.insert(PathBuf::from(path), (label.to_owned(), digest));
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        _tmp: tempfile::TempDir,
        ws: PathBuf,
        cas: Cas,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let cas = Cas::open(ws.join(".anneal/cas")).unwrap();
        Fixture { _tmp: tmp, ws, cas }
    }

    fn open(f: &Fixture) -> MaterializeStore {
        MaterializeStore::open(f.ws.join(".anneal"), &f.ws).unwrap()
    }

    fn writable(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn apply_writes_readonly_records_and_is_idempotent() {
        let f = fixture();
        let digest = f.cas.put(b"{\"port\":8080}").unwrap();
        let files = vec![(PathBuf::from("config.json"), digest)];

        let mut store = open(&f);
        let report = store.apply("//:config", &files, &f.cas, false).unwrap();
        assert_eq!(report.written, vec![PathBuf::from("config.json")]);
        assert!(report.refused.is_empty());

        let dest = f.ws.join("config.json");
        assert_eq!(fs::read(&dest).unwrap(), b"{\"port\":8080}");
        let mode = fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0, "materialized file is read-only");

        // Identical content: untouched, reported unchanged.
        let report = store.apply("//:config", &files, &f.cas, false).unwrap();
        assert_eq!(report.unchanged, vec![PathBuf::from("config.json")]);
        assert!(report.written.is_empty());

        // Manifest round-trips through reopen.
        let reopened = open(&f);
        assert_eq!(reopened.entries(), store.entries());
        assert!(reopened.paths().contains(Path::new("config.json")));
    }

    #[test]
    fn apply_rewrites_own_file_on_new_digest() {
        let f = fixture();
        let v1 = f.cas.put(b"v1").unwrap();
        let v2 = f.cas.put(b"v2").unwrap();
        let mut store = open(&f);
        store
            .apply(
                "//:config",
                &[(PathBuf::from("config.json"), v1)],
                &f.cas,
                false,
            )
            .unwrap();
        let report = store
            .apply(
                "//:config",
                &[(PathBuf::from("config.json"), v2)],
                &f.cas,
                false,
            )
            .unwrap();
        assert_eq!(report.written, vec![PathBuf::from("config.json")]);
        assert_eq!(fs::read(f.ws.join("config.json")).unwrap(), b"v2");
    }

    #[test]
    fn apply_refuses_user_edit_unless_forced() {
        let f = fixture();
        let v1 = f.cas.put(b"v1").unwrap();
        let v2 = f.cas.put(b"v2").unwrap();
        let files_v2 = vec![(PathBuf::from("config.json"), v2)];
        let mut store = open(&f);
        store
            .apply(
                "//:config",
                &[(PathBuf::from("config.json"), v1)],
                &f.cas,
                false,
            )
            .unwrap();

        let dest = f.ws.join("config.json");
        writable(&dest);
        fs::write(&dest, b"user edit").unwrap();

        let report = store.apply("//:config", &files_v2, &f.cas, false).unwrap();
        assert_eq!(report.refused.len(), 1);
        assert_eq!(fs::read(&dest).unwrap(), b"user edit");

        let report = store.apply("//:config", &files_v2, &f.cas, true).unwrap();
        assert_eq!(report.written, vec![PathBuf::from("config.json")]);
        assert_eq!(fs::read(&dest).unwrap(), b"v2");
    }

    #[test]
    fn apply_refuses_foreign_file_but_adopts_identical_content() {
        let f = fixture();
        let digest = f.cas.put(b"content").unwrap();
        let files = vec![(PathBuf::from("config.json"), digest)];

        // Pre-existing file with different content, never written by anneal.
        fs::write(f.ws.join("config.json"), b"theirs").unwrap();
        let mut store = open(&f);
        let report = store.apply("//:config", &files, &f.cas, false).unwrap();
        assert_eq!(report.refused.len(), 1);
        assert!(store.entries().is_empty());

        // Pre-existing file with identical content: adopted, not rewritten.
        fs::write(f.ws.join("config.json"), b"content").unwrap();
        let report = store.apply("//:config", &files, &f.cas, false).unwrap();
        assert_eq!(report.unchanged, vec![PathBuf::from("config.json")]);
        assert_eq!(store.entries().len(), 1);
    }

    #[test]
    fn apply_prunes_orphans_with_digest_guard() {
        let f = fixture();
        let a = f.cas.put(b"a").unwrap();
        let b = f.cas.put(b"b").unwrap();
        let mut store = open(&f);
        store
            .apply("//:config", &[(PathBuf::from("a.json"), a)], &f.cas, false)
            .unwrap();

        // The target now provides b.json instead: a.json is pruned.
        let report = store
            .apply("//:config", &[(PathBuf::from("b.json"), b)], &f.cas, false)
            .unwrap();
        assert_eq!(report.pruned, vec![PathBuf::from("a.json")]);
        assert!(!f.ws.join("a.json").exists());
        assert_eq!(store.paths().len(), 1);

        // An edited orphan is left in place (and stays in the manifest).
        let dest = f.ws.join("b.json");
        writable(&dest);
        fs::write(&dest, b"edited").unwrap();
        let report = store.apply("//:config", &[], &f.cas, false).unwrap();
        assert_eq!(report.refused.len(), 1);
        assert!(dest.exists());
        assert_eq!(store.paths().len(), 1);
    }

    #[test]
    fn apply_refuses_cross_label_collision() {
        let f = fixture();
        let digest = f.cas.put(b"x").unwrap();
        let other = f.cas.put(b"y").unwrap();
        let mut store = open(&f);
        store
            .apply(
                "//:one",
                &[(PathBuf::from("out.json"), digest)],
                &f.cas,
                false,
            )
            .unwrap();
        let report = store
            .apply(
                "//:two",
                &[(PathBuf::from("out.json"), other)],
                &f.cas,
                false,
            )
            .unwrap();
        assert_eq!(report.refused.len(), 1);
        assert!(report.refused[0].reason.contains("//:one"));
    }

    #[test]
    fn clean_removes_intact_skips_edited_and_filters_by_label() {
        let f = fixture();
        let a = f.cas.put(b"a").unwrap();
        let b = f.cas.put(b"b").unwrap();
        let mut store = open(&f);
        store
            .apply("//:a", &[(PathBuf::from("a.json"), a)], &f.cas, false)
            .unwrap();
        store
            .apply("//:b", &[(PathBuf::from("b.json"), b)], &f.cas, false)
            .unwrap();

        // Label-filtered clean touches only that label's entries.
        let report = store.clean(Some("//:a"), false).unwrap();
        assert_eq!(report.removed, vec![PathBuf::from("a.json")]);
        assert!(!f.ws.join("a.json").exists());
        assert!(f.ws.join("b.json").exists());

        // An edited file survives clean (without force) and keeps its entry.
        let dest = f.ws.join("b.json");
        writable(&dest);
        fs::write(&dest, b"edited").unwrap();
        let report = store.clean(None, false).unwrap();
        assert_eq!(report.refused.len(), 1);
        assert!(dest.exists());
        assert_eq!(store.entries().len(), 1);

        let report = store.clean(None, true).unwrap();
        assert_eq!(report.removed, vec![PathBuf::from("b.json")]);
        assert!(!dest.exists());
        assert!(store.entries().is_empty());
    }

    #[test]
    fn check_reports_fresh_stale_and_orphans() {
        let f = fixture();
        let v1 = f.cas.put(b"v1").unwrap();
        let v2 = f.cas.put(b"v2").unwrap();
        let mut store = open(&f);
        store
            .apply(
                "//:config",
                &[(PathBuf::from("config.json"), v1)],
                &f.cas,
                false,
            )
            .unwrap();

        let report = store
            .check("//:config", &[(PathBuf::from("config.json"), v1)])
            .unwrap();
        assert_eq!(report.fresh, vec![PathBuf::from("config.json")]);
        assert!(report.stale.is_empty());

        // Provided digest moved on: stale. The old entry is also an orphan if
        // the destination changed.
        let report = store
            .check("//:config", &[(PathBuf::from("config.json"), v2)])
            .unwrap();
        assert_eq!(report.stale, vec![PathBuf::from("config.json")]);

        let report = store
            .check("//:config", &[(PathBuf::from("renamed.json"), v1)])
            .unwrap();
        assert_eq!(report.stale.len(), 2); // missing renamed.json + orphaned config.json
    }

    #[test]
    fn tree_state_distinguishes_intact_edited_missing() {
        let f = fixture();
        let digest = f.cas.put(b"x").unwrap();
        let mut store = open(&f);
        store
            .apply("//:t", &[(PathBuf::from("x.json"), digest)], &f.cas, false)
            .unwrap();
        let entry = &store.entries()[0];
        assert_eq!(store.tree_state(entry).unwrap(), TreeState::Intact);

        let dest = f.ws.join("x.json");
        writable(&dest);
        fs::write(&dest, b"edited").unwrap();
        assert_eq!(store.tree_state(entry).unwrap(), TreeState::Edited);

        fs::remove_file(&dest).unwrap();
        assert_eq!(store.tree_state(entry).unwrap(), TreeState::Missing);
    }

    #[test]
    fn rejects_escaping_destinations() {
        let f = fixture();
        let digest = f.cas.put(b"x").unwrap();
        let mut store = open(&f);
        for bad in ["../escape.json", "/abs.json"] {
            let err = store
                .apply("//:t", &[(PathBuf::from(bad), digest)], &f.cas, false)
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn subdirectory_destinations_create_parents() {
        let f = fixture();
        let digest = f.cas.put(b"x").unwrap();
        let mut store = open(&f);
        store
            .apply(
                "//:t",
                &[(PathBuf::from("gen/deep/x.json"), digest)],
                &f.cas,
                false,
            )
            .unwrap();
        assert_eq!(fs::read(f.ws.join("gen/deep/x.json")).unwrap(), b"x");
    }
}
