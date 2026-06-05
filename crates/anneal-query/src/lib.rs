//! Graph queries (§11): ownership, `affected`, and `why`.
//!
//! Milestone-1 scope: [`owner`] (the file → package map) and [`affected`] (the
//! reverse-dependency closure of a change). `why` arrives next.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use anneal_core::Label;
use anneal_loader::TargetGraph;

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

/// The result of an [`affected`] query.
pub struct Affected {
    /// The affected targets, sorted and deduped: every target in a changed package, plus
    /// everything that transitively depends on one.
    pub targets: Vec<Label>,
    /// Set when a changed file had no owning package, forcing a conservative
    /// workspace-wide result (§11.3). `targets` is then *every* target in the graph.
    pub workspace_wide: bool,
    /// The unowned changed files that triggered `workspace_wide` (for diagnostics).
    pub unowned: Vec<PathBuf>,
}

/// The targets **affected** by a set of changed files (§11.3): every target in a changed
/// file's package, plus the reverse-dependency closure of those targets.
///
/// Package granularity (a changed file affects *all* targets in its package) is the sound,
/// cheap choice — it needs only the loaded graph, never analysis, and never *under*-selects
/// (the correctness requirement; over-selection is recovered by the cache layers below).
/// A changed file with **no owning package** can't be scoped, so it conservatively makes
/// the whole workspace affected.
///
/// Pure: `changed` is supplied by the caller (the CLI runs `git diff`), so this is
/// testable without git. `workspace_root` is needed only to resolve [`owner`].
pub fn affected(workspace_root: &Path, graph: &TargetGraph, changed: &[PathBuf]) -> Affected {
    // 1. Map each changed file to its owning package; collect any unowned files.
    let mut changed_packages: BTreeSet<String> = BTreeSet::new();
    let mut unowned: Vec<PathBuf> = Vec::new();
    for path in changed {
        match owner(workspace_root, path) {
            Some(package) => {
                changed_packages.insert(package);
            }
            None => unowned.push(path.clone()),
        }
    }

    // 2. An unowned change can't be scoped → conservatively, everything is affected.
    if !unowned.is_empty() {
        let mut targets: Vec<Label> = graph.targets().map(|t| t.label.clone()).collect();
        targets.sort();
        return Affected {
            targets,
            workspace_wide: true,
            unowned,
        };
    }

    // 3. Seeds: every target in a changed package (package granularity).
    let seeds: Vec<Label> = graph
        .targets()
        .filter(|t| changed_packages.contains(t.label.package()))
        .map(|t| t.label.clone())
        .collect();

    // 4. Reverse-dependency index: dep → the targets that declared it.
    let mut rdeps: HashMap<&Label, Vec<&Label>> = HashMap::new();
    for target in graph.targets() {
        for dep in &target.deps {
            rdeps.entry(dep).or_default().push(&target.label);
        }
    }

    // 5. Reverse-closure from the seeds (a seed is itself affected).
    let mut reached: BTreeSet<Label> = BTreeSet::new();
    let mut stack = seeds;
    while let Some(label) = stack.pop() {
        if reached.insert(label.clone()) {
            if let Some(dependents) = rdeps.get(&label) {
                stack.extend(dependents.iter().map(|l| (*l).clone()));
            }
        }
    }

    Affected {
        targets: reached.into_iter().collect(), // BTreeSet → sorted, deduped
        workspace_wide: false,
        unowned: Vec::new(),
    }
}

/// A shortest forward-dependency path from `from` to **any** target in `targets`, or
/// `None` if none is reachable. The path includes both endpoints (`from` first, the
/// matched target last); if `from` is itself in `targets`, the path is just `[from]`.
///
/// **Deterministic and stable under cosmetic dep reordering:** the BFS explores each
/// node's dependencies in **sorted-label order** and uses maps only for membership /
/// parent lookup (never to *order* exploration), so the chosen path is independent of
/// declaration order and `HashMap` iteration order. Among equal-length paths, the one
/// through the earlier-sorting dependency wins.
pub fn shortest_path(
    graph: &TargetGraph,
    from: &Label,
    targets: &BTreeSet<Label>,
) -> Option<Vec<Label>> {
    if targets.contains(from) {
        return Some(vec![from.clone()]);
    }
    let mut parent: HashMap<Label, Label> = HashMap::new();
    let mut visited: BTreeSet<Label> = BTreeSet::new();
    let mut queue: VecDeque<Label> = VecDeque::new();
    visited.insert(from.clone());
    queue.push_back(from.clone());

    while let Some(node) = queue.pop_front() {
        let Some(decl) = graph.get(&node) else {
            continue;
        };
        let mut deps: Vec<&Label> = decl.deps.iter().collect();
        deps.sort();
        for dep in deps {
            if !visited.insert(dep.clone()) {
                continue;
            }
            parent.insert(dep.clone(), node.clone());
            if targets.contains(dep) {
                return Some(reconstruct(&parent, from, dep));
            }
            queue.push_back(dep.clone());
        }
    }
    None
}

/// A shortest dependency path from `from` to `to` (`why <from> <to>`), or `None`.
pub fn why(graph: &TargetGraph, from: &Label, to: &Label) -> Option<Vec<Label>> {
    let mut target = BTreeSet::new();
    target.insert(to.clone());
    shortest_path(graph, from, &target)
}

/// Reconstruct the path `from → … → to` by following parent pointers back from `to`.
fn reconstruct(parent: &HashMap<Label, Label>, from: &Label, to: &Label) -> Vec<Label> {
    let mut path = vec![to.clone()];
    let mut cur = to.clone();
    while &cur != from {
        let p = parent[&cur].clone();
        path.push(p.clone());
        cur = p;
    }
    path.reverse();
    path
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
        assert_eq!(
            own(root, "crates/lib/src/x.rs").as_deref(),
            Some("crates/lib")
        );
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
        assert_eq!(
            own(ws.path(), "app/was/here/gone.rs").as_deref(),
            Some("app")
        );
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

#[cfg(test)]
mod affected_tests {
    use super::*;
    use anneal_loader::load_workspace;
    use anneal_rules::builtin_rules;

    /// `app → lib → base`, plus an independent `other`. `data`-style genrule deps carry
    /// the edges the reverse-closure walks.
    fn diamond_workspace() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let write = |pkg: &str, build: &str| {
            let dir = tmp.path().join(pkg);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("BUILD"), build).unwrap();
        };
        write(
            "base",
            "genrule(name = \"base\", outs = [\"b\"], cmd = \"echo > $(OUTS)\")\n",
        );
        write("lib", "genrule(name = \"lib\", deps = [\"//base:base\"], outs = [\"l\"], cmd = \"echo > $(OUTS)\")\n");
        write("app", "genrule(name = \"app\", deps = [\"//lib:lib\"], outs = [\"a\"], cmd = \"echo > $(OUTS)\")\n");
        write(
            "other",
            "genrule(name = \"other\", outs = [\"o\"], cmd = \"echo > $(OUTS)\")\n",
        );
        tmp
    }

    fn affected_labels(root: &Path, graph: &TargetGraph, changed: &[&str]) -> Vec<String> {
        let paths: Vec<PathBuf> = changed.iter().map(PathBuf::from).collect();
        affected(root, graph, &paths)
            .targets
            .iter()
            .map(|l| l.to_string())
            .collect()
    }

    #[test]
    fn change_propagates_up_the_reverse_closure() {
        let tmp = diamond_workspace();
        let graph = load_workspace(tmp.path(), &builtin_rules()).unwrap();

        // Editing base affects base and everything that (transitively) depends on it.
        assert_eq!(
            affected_labels(tmp.path(), &graph, &["base/src.txt"]),
            vec!["//app:app", "//base:base", "//lib:lib"],
        );
        // Editing a leaf consumer affects only itself.
        assert_eq!(
            affected_labels(tmp.path(), &graph, &["app/src.txt"]),
            vec!["//app:app"],
        );
        // An independent package is isolated.
        assert_eq!(
            affected_labels(tmp.path(), &graph, &["other/src.txt"]),
            vec!["//other:other"],
        );
    }

    #[test]
    fn unowned_change_is_conservatively_workspace_wide() {
        let tmp = diamond_workspace(); // no root BUILD → a root-level file is unowned
        let graph = load_workspace(tmp.path(), &builtin_rules()).unwrap();

        let result = affected(tmp.path(), &graph, &[PathBuf::from("flake.nix")]);
        assert!(
            result.workspace_wide,
            "an unowned change forces workspace-wide"
        );
        assert_eq!(result.unowned, vec![PathBuf::from("flake.nix")]);
        // Every target is affected.
        assert_eq!(result.targets.len(), 4);
    }
}

#[cfg(test)]
mod why_tests {
    use super::*;
    use anneal_loader::load_workspace;
    use anneal_rules::builtin_rules;

    fn label(s: &str) -> Label {
        Label::parse(s).unwrap()
    }

    fn path_str(graph: &TargetGraph, from: &str, to: &str) -> Option<Vec<String>> {
        why(graph, &label(from), &label(to)).map(|p| p.iter().map(|l| l.to_string()).collect())
    }

    #[test]
    fn finds_a_path_and_reports_none_when_unreachable() {
        let tmp = tempfile::tempdir().unwrap();
        let write = |pkg: &str, build: &str| {
            let dir = tmp.path().join(pkg);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("BUILD"), build).unwrap();
        };
        write(
            "base",
            "genrule(name = \"base\", outs = [\"b\"], cmd = \"echo > $(OUTS)\")\n",
        );
        write("lib", "genrule(name = \"lib\", deps = [\"//base:base\"], outs = [\"l\"], cmd = \"echo > $(OUTS)\")\n");
        write("app", "genrule(name = \"app\", deps = [\"//lib:lib\"], outs = [\"a\"], cmd = \"echo > $(OUTS)\")\n");
        write(
            "other",
            "genrule(name = \"other\", outs = [\"o\"], cmd = \"echo > $(OUTS)\")\n",
        );
        let graph = load_workspace(tmp.path(), &builtin_rules()).unwrap();

        assert_eq!(
            path_str(&graph, "//app:app", "//base:base"),
            Some(vec![
                "//app:app".into(),
                "//lib:lib".into(),
                "//base:base".into()
            ]),
        );
        // No path to an independent target.
        assert_eq!(path_str(&graph, "//app:app", "//other:other"), None);
        // A target trivially reaches itself.
        assert_eq!(
            path_str(&graph, "//app:app", "//app:app"),
            Some(vec!["//app:app".into()])
        );
    }

    #[test]
    fn tie_break_is_deterministic_via_sorted_labels() {
        // `app` reaches `base` through two equal-length routes (`ma` and `mz`). The
        // sorted-label rule must always pick the one through the earlier label (`ma`),
        // regardless of the order deps are declared in the BUILD file.
        let tmp = tempfile::tempdir().unwrap();
        let write = |pkg: &str, build: &str| {
            let dir = tmp.path().join(pkg);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("BUILD"), build).unwrap();
        };
        write(
            "base",
            "genrule(name = \"base\", outs = [\"b\"], cmd = \"echo > $(OUTS)\")\n",
        );
        write("ma", "genrule(name = \"m\", deps = [\"//base:base\"], outs = [\"x\"], cmd = \"echo > $(OUTS)\")\n");
        write("mz", "genrule(name = \"m\", deps = [\"//base:base\"], outs = [\"x\"], cmd = \"echo > $(OUTS)\")\n");
        // Declare deps in NON-sorted order (mz before ma) to prove the result is
        // independent of declaration order.
        write("app", "genrule(name = \"app\", deps = [\"//mz:m\", \"//ma:m\"], outs = [\"a\"], cmd = \"echo > $(OUTS)\")\n");
        let graph = load_workspace(tmp.path(), &builtin_rules()).unwrap();

        assert_eq!(
            path_str(&graph, "//app:app", "//base:base"),
            Some(vec![
                "//app:app".into(),
                "//ma:m".into(),
                "//base:base".into()
            ]),
            "sorted-label tie-break picks the `ma` route despite `mz` being declared first",
        );
    }
}
