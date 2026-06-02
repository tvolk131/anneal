//! Graph queries (§11): ownership, `affected`, and `why`.
//!
//! Milestone-1 scope: [`owner`] (the file → package map that `affected` builds on). The
//! `affected` and `why` queries layer on top and arrive in subsequent increments.

use std::path::Path;

/// The package that **owns** a path: the nearest enclosing directory containing a
/// `BUILD` file (§1.5). Returns the package path (`/`-separated; empty string for the
/// root package), or `None` if no `BUILD` ancestor exists within the workspace.
///
/// `rel_path` is workspace-relative. Ownership is resolved at the **package** level (not
/// a target): a source file has no single owning target — it is shared usage — but it
/// has exactly one nearest enclosing package, which makes `owner` total and cheap (a
/// filesystem walk, no graph). `affected` then expands a package into its targets.
///
/// Operates on the **path string**, not a live file, so it resolves owners for *deleted*
/// files in a diff: the `BUILD` ancestor still exists even when the file is gone.
pub fn owner(workspace_root: &Path, rel_path: &Path) -> Option<String> {
    // Walk up from the file's containing directory toward the root; the file itself is
    // never a package, so we start at its parent.
    let mut current = rel_path.parent();
    while let Some(dir) = current {
        if workspace_root.join(dir).join("BUILD").is_file() {
            return Some(package_path(dir));
        }
        current = dir.parent();
    }
    None
}

/// A relative path as a `/`-separated package path (empty string for the root package).
fn package_path(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A workspace with `BUILD` files at the root, `app/`, and the nested `app/sub/`.
    fn workspace() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for pkg in ["", "app", "app/sub", "crates/lib"] {
            let dir = tmp.path().join(pkg);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("BUILD"), "").unwrap();
        }
        // A directory with no BUILD anywhere above it (other than... the root has one).
        std::fs::create_dir_all(tmp.path().join("docs")).unwrap();
        tmp
    }

    fn own(root: &Path, p: &str) -> Option<String> {
        owner(root, &PathBuf::from(p))
    }

    #[test]
    fn nearest_enclosing_package_wins() {
        let ws = workspace();
        let root = ws.path();
        assert_eq!(own(root, "app/src/lib.rs").as_deref(), Some("app"));
        // The nested package shadows its parent.
        assert_eq!(own(root, "app/sub/mod.rs").as_deref(), Some("app/sub"));
        assert_eq!(own(root, "crates/lib/src/x.rs").as_deref(), Some("crates/lib"));
    }

    #[test]
    fn falls_back_to_the_root_package() {
        let ws = workspace();
        // `docs/` has no BUILD, but the root does, so the root package owns it.
        assert_eq!(own(ws.path(), "docs/readme.md").as_deref(), Some(""));
        assert_eq!(own(ws.path(), "top.txt").as_deref(), Some(""));
    }

    #[test]
    fn deleted_file_path_still_resolves() {
        let ws = workspace();
        // The file does not exist; ownership is by path, so `app` still owns it.
        assert_eq!(own(ws.path(), "app/was/here/gone.rs").as_deref(), Some("app"));
    }

    #[test]
    fn unowned_when_no_build_ancestor() {
        // A workspace with no root BUILD: a file outside every package is unowned.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("app")).unwrap();
        std::fs::write(tmp.path().join("app/BUILD"), "").unwrap();
        std::fs::create_dir_all(tmp.path().join("loose")).unwrap();
        assert_eq!(own(tmp.path(), "loose/x.txt"), None);
        assert_eq!(own(tmp.path(), "app/x.rs").as_deref(), Some("app"));
    }
}
