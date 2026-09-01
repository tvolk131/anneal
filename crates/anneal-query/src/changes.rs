//! The VCS seam: what changed in this working tree (§11).
//!
//! Every question of the form *"which files differ from X?"* lives here — the
//! one module that talks to git — so `affected`, `why --since`, and the focus
//! cone all share one answer to it. The graph side of those queries stays in
//! the crate root; this module only produces **changed-file sets**:
//!
//! - [`changed_since`]: files differing from a literal ref (`git diff`).
//! - [`changed_base`]: files differing from where `HEAD` diverged from a ref
//!   (`git merge-base` + diff) — the CI-safe form. Diffing against the ref
//!   *itself* silently includes the branch's *stale* distance from the target
//!   branch (a `merge-base` shape fix; the classic cone-inflation footgun).
//! - [`untracked`]: files git has never tracked and no ignore rule covers —
//!   present in the working tree, invisible to `git diff`. Including them is
//!   what closes the untracked gap without sweeping ignored build junk.
//! - [`dirty`]: the whole working-tree edit horizon (staged, unstaged,
//!   untracked) — the focus cone's input.
//!
//! Deleted files appear in every diff form (ownership resolves on the path
//! string, so a deleted file still maps to its package). Rename entries
//! contribute **both** sides: the old path's owner is affected by the removal.
//!
//! Paths come back **repo-root-relative** (git's default), and callers here
//! assume the workspace root *is* the git root — a workspace nested inside a
//! larger repository would need path rebasing, which nothing supports today.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A failed git invocation: the command's stderr, or why it could not run.
#[derive(Debug)]
pub struct ChangesError(pub String);

impl std::fmt::Display for ChangesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ChangesError {}

fn git(root: &Path, args: &[&str]) -> Result<String, ChangesError> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| ChangesError(format!("running git {args:?}: {e}")))?;
    if !out.status.success() {
        return Err(ChangesError(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn lines(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Files that differ from `git_ref` (a literal ref: branch, tag, SHA). Does
/// not include untracked files — compose with [`untracked`] when the working
/// tree's never-committed files matter.
pub fn changed_since(root: &Path, git_ref: &str) -> Result<Vec<PathBuf>, ChangesError> {
    Ok(lines(&git(root, &["diff", "--name-only", git_ref])?))
}

/// Where `HEAD` diverged from `git_ref`, and the files changed since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedBase {
    /// The merge-base commit, as a SHA — the point the two histories share.
    pub base: String,
    /// Files changed from `base` to the working tree.
    pub files: Vec<PathBuf>,
}

/// Files changed since `HEAD` diverged from `git_ref` — the CI form. On a
/// pull-request checkout this is exactly the PR's change set, regardless of
/// how far the target branch has moved underneath it.
pub fn changed_base(root: &Path, git_ref: &str) -> Result<ChangedBase, ChangesError> {
    let base = git(root, &["merge-base", git_ref, "HEAD"])?
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if base.is_empty() {
        return Err(ChangesError(format!(
            "no common ancestor between {git_ref} and HEAD"
        )));
    }
    let files = changed_since(root, &base)?;
    Ok(ChangedBase { base, files })
}

/// Files present in the working tree that git has never tracked **and no
/// ignore rule covers** (`--exclude-standard`): a brand-new source file shows
/// up; `target/` and other ignored debris never do. Anneal's own store
/// (`.anneal/`) is filtered structurally as well — a repo that hasn't
/// gitignored it yet would otherwise flip every change query workspace-wide
/// after its first build.
pub fn untracked(root: &Path) -> Result<Vec<PathBuf>, ChangesError> {
    Ok(
        lines(&git(root, &["ls-files", "--others", "--exclude-standard"])?)
            .into_iter()
            .filter(|p| !is_anneal_store(p))
            .collect(),
    )
}

/// Whether a path is inside anneal's store directory (`.anneal/…`).
fn is_anneal_store(path: &Path) -> bool {
    path.components()
        .next()
        .is_some_and(|c| c.as_os_str() == ".anneal")
}

/// The dirty working tree — staged, unstaged, and untracked — as
/// workspace-relative paths. The focus cone's edit horizon. Rename entries
/// (`R  old -> new`) contribute both sides: the old path's owner is affected
/// by the removal, the new path's by the addition.
pub fn dirty(root: &Path) -> Result<Vec<PathBuf>, ChangesError> {
    let text = git(root, &["status", "--porcelain"])?;
    Ok(parse_porcelain(&text))
}

/// Parse `git status --porcelain` output into paths.
fn parse_porcelain(text: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for line in text.lines() {
        if line.len() < 4 {
            continue;
        }
        let rest = &line[3..];
        match rest.split_once(" -> ") {
            Some((old, new)) => {
                paths.push(PathBuf::from(old.trim()));
                paths.push(PathBuf::from(new.trim()));
            }
            None => paths.push(PathBuf::from(rest.trim())),
        }
    }
    paths
}

/// The subset of `paths` tracked by git (empty when not a git repo — then
/// nothing is tracked and callers proceed without the guard).
pub fn tracked<'p>(root: &Path, paths: impl Iterator<Item = &'p Path>) -> Vec<PathBuf> {
    let mut cmd = Command::new("git");
    cmd.args(["ls-files", "-z", "--"]).current_dir(root);
    let mut any = false;
    for path in paths {
        cmd.arg(path);
        any = true;
    }
    if !any {
        return Vec::new();
    }
    match cmd.output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether git's ignore rules cover `path` (true = ignored). False when not a
/// git repo or on any git failure — the same "no answer means not ignored"
/// posture as [`tracked`].
pub fn ignored(root: &Path, path: &Path) -> bool {
    Command::new("git")
        .args(["check-ignore", "-q", "--"])
        .arg(path)
        .current_dir(root)
        .status()
        .ok()
        .and_then(|s| s.code())
        == Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A real git repo with one commit on `main` and a diverged `feature`
    /// branch, so merge-base resolution has actual histories to work over.
    struct Repo {
        _tmp: tempfile::TempDir,
        root: PathBuf,
    }

    fn git_ok(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(root)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn repo() -> Repo {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        git_ok(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("base.txt"), "base\n").unwrap();
        git_ok(&root, &["add", "."]);
        git_ok(&root, &["commit", "-q", "-m", "base"]);
        Repo { _tmp: tmp, root }
    }

    #[test]
    fn changed_since_is_the_literal_diff() {
        let r = repo();
        fs::write(r.root.join("base.txt"), "edited\n").unwrap();
        fs::write(r.root.join("new.txt"), "new\n").unwrap();
        git_ok(&r.root, &["add", "."]);
        git_ok(&r.root, &["commit", "-q", "-m", "edit"]);
        assert_eq!(
            changed_since(&r.root, "HEAD~1").unwrap(),
            vec![PathBuf::from("base.txt"), PathBuf::from("new.txt")]
        );
    }

    #[test]
    fn changed_base_resolves_the_merge_base_not_the_ref_tip() {
        // The wedge-critical property: after the target branch moves *forward*
        // past the divergence point, --base still reports only this branch's
        // changes. Diffing against the ref tip would (wrongly) include the
        // target branch's own new files as "changes".
        let r = repo();
        git_ok(&r.root, &["checkout", "-q", "-b", "feature"]);
        fs::write(r.root.join("feature.txt"), "f\n").unwrap();
        git_ok(&r.root, &["add", "."]);
        git_ok(&r.root, &["commit", "-q", "-m", "feature work"]);
        // The target branch moves on without us.
        git_ok(&r.root, &["checkout", "-q", "main"]);
        fs::write(r.root.join("upstream.txt"), "u\n").unwrap();
        git_ok(&r.root, &["add", "."]);
        git_ok(&r.root, &["commit", "-q", "-m", "upstream moves"]);
        git_ok(&r.root, &["checkout", "-q", "feature"]);

        let cb = changed_base(&r.root, "main").unwrap();
        assert_eq!(cb.files, vec![PathBuf::from("feature.txt")]);
        assert!(!cb.base.is_empty());
        // The literal diff, for contrast, would include upstream.txt.
        assert!(changed_since(&r.root, "main")
            .unwrap()
            .contains(&PathBuf::from("upstream.txt")));
    }

    #[test]
    fn untracked_lists_new_files_but_not_ignored_debris() {
        let r = repo();
        fs::write(r.root.join(".gitignore"), "ignored-dir/\n*.log\n").unwrap();
        git_ok(&r.root, &["add", "."]);
        git_ok(&r.root, &["commit", "-q", "-m", "gitignore"]);
        fs::write(r.root.join("brand-new.txt"), "n\n").unwrap();
        fs::write(r.root.join("noise.log"), "n\n").unwrap();
        fs::create_dir_all(r.root.join("ignored-dir")).unwrap();
        fs::write(r.root.join("ignored-dir/x"), "n\n").unwrap();

        assert_eq!(
            untracked(&r.root).unwrap(),
            vec![PathBuf::from("brand-new.txt")],
            "untracked must include new sources and exclude ignored junk"
        );
    }

    #[test]
    fn untracked_excludes_the_anneal_store_itself() {
        // Without this, the first build (which creates `.anneal/`) would turn
        // every later change query workspace-wide in un-gitignored repos.
        let r = repo();
        fs::create_dir_all(r.root.join(".anneal/store/objects")).unwrap();
        fs::write(r.root.join(".anneal/store/objects/blob"), b"x").unwrap();
        fs::write(r.root.join("real.txt"), "r\n").unwrap();
        assert_eq!(untracked(&r.root).unwrap(), vec![PathBuf::from("real.txt")]);
    }

    #[test]
    fn dirty_covers_modified_untracked_and_renames() {
        let r = repo();
        fs::write(r.root.join("base.txt"), "edited\n").unwrap();
        fs::write(r.root.join("fresh.txt"), "fresh\n").unwrap();
        let dirty = dirty(&r.root).unwrap();
        assert!(dirty.contains(&PathBuf::from("base.txt")));
        assert!(dirty.contains(&PathBuf::from("fresh.txt")));
    }

    #[test]
    fn porcelain_renames_contribute_both_sides() {
        let parsed = parse_porcelain("R  old/name.rs -> new/name.rs\n?? added\n");
        assert_eq!(
            parsed,
            vec![
                PathBuf::from("old/name.rs"),
                PathBuf::from("new/name.rs"),
                PathBuf::from("added"),
            ]
        );
    }

    #[test]
    fn tracked_filters_and_ignored_answers() {
        let r = repo();
        fs::write(r.root.join("untracked.txt"), "u\n").unwrap();
        let tracked = tracked(
            &r.root,
            [Path::new("base.txt"), Path::new("untracked.txt")].into_iter(),
        );
        assert_eq!(tracked, vec![PathBuf::from("base.txt")]);
        assert!(!ignored(&r.root, Path::new("base.txt")));
    }

    #[test]
    fn a_missing_ref_is_a_clear_error() {
        let r = repo();
        let err = changed_base(&r.root, "no-such-ref").unwrap_err();
        assert!(err.to_string().contains("no-such-ref"), "{err}");
    }
}
