//! The analysis context the system hands to a rule.
//!
//! [`RuleContext`] is the rule's entire view of the world: its label, typed
//! attributes, configuration, the providers of its already-analyzed dependencies,
//! and a source-file resolver. A rule cannot reach outside this — it can't read
//! arbitrary files or inspect global state — which keeps the system/rule boundary
//! sharp.

use std::path::{Component, Path, PathBuf};

use anneal_cas::Cas;
use anneal_core::{Configuration, Label};

use crate::attrs::Attrs;
use crate::providers::{Artifact, ArtifactSource, ProviderSet};
use crate::rule::RuleError;

/// A dependency that has already been analyzed: its label and the providers it
/// exposed.
#[derive(Debug, Clone)]
pub struct ResolvedDep {
    pub label: Label,
    pub providers: ProviderSet,
}

/// Everything a rule may see while analyzing one configured target.
pub struct RuleContext<'a> {
    label: Label,
    attrs: &'a Attrs,
    config: &'a Configuration,
    package_dir: &'a Path,
    cas: &'a Cas,
    deps: &'a [ResolvedDep],
}

impl<'a> RuleContext<'a> {
    pub fn new(
        label: Label,
        attrs: &'a Attrs,
        config: &'a Configuration,
        package_dir: &'a Path,
        cas: &'a Cas,
        deps: &'a [ResolvedDep],
    ) -> Self {
        RuleContext {
            label,
            attrs,
            config,
            package_dir,
            cas,
            deps,
        }
    }

    pub fn label(&self) -> &Label {
        &self.label
    }

    pub fn attrs(&self) -> &Attrs {
        self.attrs
    }

    pub fn config(&self) -> &Configuration {
        self.config
    }

    pub fn deps(&self) -> &[ResolvedDep] {
        self.deps
    }

    /// Resolve a source file (path relative to the package) into a content-addressed
    /// [`Artifact`], reading it into the CAS. This is the system performing the I/O
    /// on the rule's behalf — the rule never touches the filesystem directly.
    pub fn source_artifact(&self, rel: &Path) -> Result<Artifact, RuleError> {
        let rel = package_relative_path(rel, "source path", false)?;
        let abs = self.package_dir.join(&rel);
        let digest = self
            .cas
            .ingest_file(&abs)
            .map_err(|error| RuleError::Source {
                path: rel.clone(),
                error,
            })?;
        Ok(Artifact {
            path: rel,
            source: ArtifactSource::Source(digest),
        })
    }

    /// Read a file within the package for **introspection** (e.g. parsing
    /// `Cargo.toml` to enumerate crates). Unlike [`source_artifact`], this does not
    /// add the file to the CAS as a build input — it is metadata the rule consults
    /// while deciding what actions to emit. Scoped to the package directory.
    ///
    /// [`source_artifact`]: RuleContext::source_artifact
    pub fn read_package_file(&self, rel: &Path) -> Result<String, RuleError> {
        let rel = package_relative_path(rel, "package file path", false)?;
        std::fs::read_to_string(self.package_dir.join(&rel))
            .map_err(|error| RuleError::Source { path: rel, error })
    }

    /// Whether a file exists within the package (introspection helper).
    pub fn package_file_exists(&self, rel: &Path) -> bool {
        let Ok(rel) = package_relative_path(rel, "package file path", true) else {
            return false;
        };
        self.package_dir.join(rel).exists()
    }

    /// List the immediate entries under `rel` (relative to the package), returned as
    /// paths relative to the package directory and sorted for determinism. Empty if `rel`
    /// is absent. Used to expand glob workspace members (e.g. `crates/*`).
    pub fn list_dir(&self, rel: &Path) -> Result<Vec<PathBuf>, RuleError> {
        let rel = package_relative_path(rel, "directory path", true)?;
        let base = self.package_dir.join(&rel);
        let entries = match std::fs::read_dir(&base) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(RuleError::Source { path: rel, error }),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| RuleError::Source {
                path: rel.clone(),
                error,
            })?;
            out.push(rel.join(entry.file_name()));
        }
        out.sort();
        Ok(out)
    }

    /// Resolve an entire source tree under `rel` (relative to the package) into
    /// content-addressed [`Artifact`]s, skipping directories named in `ignore_dirs`.
    /// Each artifact's `path` is relative to the package directory, so the tree
    /// materializes back into the same layout inside the sandbox. This is how a
    /// whole-package wrapper rule (e.g. `cargo_workspace`) captures its inputs.
    pub fn source_tree(
        &self,
        rel: &Path,
        ignore_dirs: &[&str],
    ) -> Result<Vec<Artifact>, RuleError> {
        let rel = package_relative_path(rel, "source tree path", true)?;
        let base = self.package_dir.join(&rel);
        let mut artifacts = Vec::new();
        self.walk_tree(&base, ignore_dirs, &mut artifacts)?;
        // Deterministic order so the resulting action is stable.
        artifacts.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(artifacts)
    }

    fn walk_tree(
        &self,
        dir: &Path,
        ignore_dirs: &[&str],
        out: &mut Vec<Artifact>,
    ) -> Result<(), RuleError> {
        let source_err = |path: &Path, error| RuleError::Source {
            path: path.to_path_buf(),
            error,
        };
        let entries = std::fs::read_dir(dir).map_err(|e| source_err(dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| source_err(dir, e))?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|e| source_err(&path, e))?;
            if file_type.is_dir() {
                let name = entry.file_name();
                if ignore_dirs
                    .iter()
                    .any(|ig| std::ffi::OsStr::new(ig) == name)
                {
                    continue;
                }
                self.walk_tree(&path, ignore_dirs, out)?;
            } else if file_type.is_file() {
                let rel = path
                    .strip_prefix(self.package_dir)
                    .unwrap_or(&path)
                    .to_path_buf();
                let digest = self
                    .cas
                    .ingest_file(&path)
                    .map_err(|e| source_err(&rel, e))?;
                out.push(Artifact {
                    path: rel,
                    source: ArtifactSource::Source(digest),
                });
            }
            // Symlinks and other entry types are skipped in Milestone 1.
        }
        Ok(())
    }
}

fn package_relative_path(rel: &Path, kind: &str, allow_dot: bool) -> Result<PathBuf, RuleError> {
    if rel.as_os_str().is_empty() {
        return Err(RuleError::Message(format!("{kind} must not be empty")));
    }
    if rel == Path::new(".") {
        return if allow_dot {
            Ok(PathBuf::from("."))
        } else {
            Err(RuleError::Message(format!("{kind} `.` is not allowed")))
        };
    }
    if rel.is_absolute() {
        return Err(RuleError::Message(format!(
            "{kind} `{}` must be package-relative",
            rel.display()
        )));
    }
    for component in rel.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {
                return Err(RuleError::Message(format!(
                    "{kind} `{}` must not contain `.` components",
                    rel.display()
                )));
            }
            Component::ParentDir => {
                return Err(RuleError::Message(format!(
                    "{kind} `{}` must not contain `..` components",
                    rel.display()
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(RuleError::Message(format!(
                    "{kind} `{}` must not contain a root or drive prefix",
                    rel.display()
                )));
            }
        }
    }
    Ok(rel.to_path_buf())
}
