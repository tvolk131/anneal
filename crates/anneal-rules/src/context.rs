//! The analysis context the system hands to a rule.
//!
//! [`RuleContext`] is the rule's entire view of the world: its label, typed
//! attributes, configuration, the providers of its already-analyzed dependencies,
//! and a source-file resolver. A rule cannot reach outside this — it can't read
//! arbitrary files or inspect global state — which keeps the system/rule boundary
//! sharp.

use std::cell::{Ref, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use anneal_cas::Cas;
use anneal_core::{Configuration, Label};
use anneal_exec::{LocalExecutor, QuerySpec};

use crate::attrs::Attrs;
use crate::providers::{Artifact, ArtifactSource, ProviderSet};
use crate::rule::RuleError;
use crate::state::{state_key, PersistentStateDecl, StateHandle};

/// A state declaration's identity: `(rule_kind, namespace, shard)` — what must
/// match bit-identically across every target that declares the same state.
type StateIdentity = (String, &'static str, Vec<String>);

/// Cross-target registry of persistent state declarations, owned by the
/// analysis run (DESIGN.md §3.3 runtime checks): `declare_state` is idempotent
/// across targets on **bit-identical** declarations and a hard error on any
/// mismatch — same identity with a different kind, attestation, shard content,
/// or paths is a fork of the trust contract, never silently resolved.
#[derive(Debug, Default)]
pub struct StateRegistry {
    declared: Mutex<BTreeMap<StateIdentity, PersistentStateDecl>>,
}

impl StateRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn check(&self, rule_kind: &str, decl: &PersistentStateDecl) -> Result<(), RuleError> {
        let mut declared = self.declared.lock().unwrap();
        let id = (rule_kind.to_owned(), decl.namespace, decl.shard.clone());
        match declared.get(&id) {
            None => {
                declared.insert(id, decl.clone());
                Ok(())
            }
            Some(existing) if existing == decl => Ok(()),
            Some(_) => Err(RuleError::Message(format!(
                "conflicting declarations for state {:?} (rule {rule_kind:?}): \
                 the same namespace+shard was declared with a different kind, \
                 attestation, or paths — state identity must be declared \
                 bit-identically by every target that uses it",
                decl.namespace
            ))),
        }
    }
}

/// A dependency that has already been analyzed: its label and the providers it
/// exposed.
#[derive(Debug, Clone)]
pub struct ResolvedDep {
    pub label: Label,
    pub providers: ProviderSet,
}

/// Source paths a rule asked the system to read while analysis was running.
#[derive(Debug, Default)]
pub struct SourcePathRecorder {
    paths: RefCell<BTreeSet<PathBuf>>,
}

impl SourcePathRecorder {
    pub fn paths(&self) -> Ref<'_, BTreeSet<PathBuf>> {
        self.paths.borrow()
    }

    pub fn record_workspace_path(&self, path: impl Into<PathBuf>) {
        self.record(path.into());
    }

    fn record(&self, path: PathBuf) {
        self.paths.borrow_mut().insert(path);
    }
}

/// Everything a rule may see while analyzing one configured target.
///
/// Capabilities are present **by construction** — the analyzer wires the full set, so a
/// rule never meets a half-built context and never has to defend against missing wiring.
/// The lone exception is [`query`](RuleContext::query): an analysis run may legitimately
/// have no executor (query-free analysis is a supported mode, e.g. graph inspection with
/// no `.anneal`), so the executor is the *only* optional capability, and asking for a
/// query without one is an honest "this run has no executor" — not a wiring bug.
pub struct RuleContext<'a> {
    label: Label,
    attrs: &'a Attrs,
    config: &'a Configuration,
    package_dir: &'a Path,
    cas: &'a Cas,
    deps: &'a [ResolvedDep],
    /// Records the source paths the rule reads (for source discovery / snapshot keys).
    source_paths: &'a SourcePathRecorder,
    /// The declaring rule's kind, for state-key scoping (§2.6).
    rule_kind: &'a str,
    /// Cross-target state-declaration registry, owned by the analysis run.
    state_registry: &'a StateRegistry,
    /// The executor, for analysis-time tool queries (§3.6). The **sole** optional
    /// capability: a query-free analysis run has none, and `query` says so honestly.
    executor: Option<&'a LocalExecutor>,
    /// Workspace-relative paths written into the tree by `anneal materialize` (empty =
    /// nothing excluded). Source discovery skips them: they are tree copies of
    /// *generated* outputs, kept only so native tools can see them — the routed action
    /// edge is the real input. Without the exclusion a materialized copy would be
    /// recorded as a source and shadow the producing action's declared output (an
    /// analysis-time hard error), and would perturb source-derived snapshot keys.
    materialized: &'a BTreeSet<PathBuf>,
}

impl<'a> RuleContext<'a> {
    /// Construct a fully-wired context. The analyzer calls this with the analysis run's
    /// source recorder, the target's rule kind, the run's state registry, and the
    /// materialize-exclusion set (empty when nothing is materialized). Enable queries by
    /// chaining [`with_executor`](Self::with_executor). Tests use [`TestContext`] to own
    /// the run-level pieces and lend a context in one call.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        label: Label,
        attrs: &'a Attrs,
        config: &'a Configuration,
        package_dir: &'a Path,
        cas: &'a Cas,
        deps: &'a [ResolvedDep],
        source_paths: &'a SourcePathRecorder,
        rule_kind: &'a str,
        state_registry: &'a StateRegistry,
        materialized: &'a BTreeSet<PathBuf>,
    ) -> Self {
        RuleContext {
            label,
            attrs,
            config,
            package_dir,
            cas,
            deps,
            source_paths,
            rule_kind,
            state_registry,
            executor: None,
            materialized,
        }
    }

    /// Enable analysis-time tool queries by wiring the executor (§3.6). The only
    /// optional capability — without it, [`query`](Self::query) reports that this
    /// analysis run has no executor.
    pub fn with_executor(mut self, executor: &'a LocalExecutor) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Declare a persistent state tree this rule's actions may use (§2.1).
    /// Returns the only mintable [`StateHandle`]; an `Interleaved` declaration
    /// cannot be constructed without an [`Attestation`](crate::Attestation), so
    /// the mutate grant provably has one behind it. Idempotent across targets
    /// on bit-identical declarations; any mismatch is a hard error.
    pub fn declare_state(&self, decl: PersistentStateDecl) -> Result<StateHandle, RuleError> {
        self.state_registry.check(self.rule_kind, &decl)?;
        Ok(StateHandle {
            key: state_key(self.rule_kind, &decl),
            kind: decl.kind,
            namespace: decl.namespace,
            paths: decl.paths,
        })
    }

    /// Run (or cache-hit) an analysis-time tool query (§3.6): a sealed,
    /// network-denied action whose captured stdout is the result. The honest
    /// form of "ask the tool" — sandboxed, keyed, cached like any action —
    /// where `read_package_file` is the pure in-process form.
    pub fn query(&self, spec: &QuerySpec) -> Result<Vec<u8>, RuleError> {
        let executor = self.executor.ok_or_else(|| {
            RuleError::Message(
                "analysis-time query requested, but this analysis run has no executor \
                 (query-free analysis is a supported mode; queries are unavailable in it)"
                    .to_owned(),
            )
        })?;
        executor
            .run_query(spec)
            .map(|result| result.stdout)
            .map_err(|e| RuleError::Message(format!("query failed: {e}")))
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
        self.record_source_path(&rel);
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
        let contents = std::fs::read_to_string(self.package_dir.join(&rel)).map_err(|error| {
            RuleError::Source {
                path: rel.clone(),
                error,
            }
        })?;
        self.record_source_path(&rel);
        Ok(contents)
    }

    /// Whether a file exists within the package (introspection helper).
    pub fn package_file_exists(&self, rel: &Path) -> bool {
        let Ok(rel) = package_relative_path(rel, "package file path", true) else {
            return false;
        };
        let exists = self.package_dir.join(&rel).exists();
        if exists {
            self.record_source_path(&rel);
        }
        exists
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
        self.record_source_path(&rel);
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| RuleError::Source {
                path: rel.clone(),
                error,
            })?;
            let entry_path = rel.join(entry.file_name());
            self.record_source_path(&entry_path);
            out.push(entry_path);
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
                if self.is_materialized(&rel) {
                    continue; // a tree copy of a generated output, not a source
                }
                let digest = self
                    .cas
                    .ingest_file(&path)
                    .map_err(|e| source_err(&rel, e))?;
                self.record_source_path(&rel);
                out.push(Artifact {
                    path: rel,
                    source: ArtifactSource::Source(digest),
                });
            }
            // Symlinks and other entry types are skipped in Milestone 1.
        }
        Ok(())
    }

    fn record_source_path(&self, rel: &Path) {
        self.source_paths
            .record(workspace_relative_path(self.label.package(), rel));
    }

    /// Whether a package-relative path is an `anneal materialize`-written tree
    /// copy (the exclusion set holds workspace-relative paths).
    fn is_materialized(&self, rel: &Path) -> bool {
        self.materialized
            .contains(&workspace_relative_path(self.label.package(), rel))
    }
}

/// Test-only owner of the **run-level** capabilities a [`RuleContext`] borrows — the
/// source recorder, the state registry, the materialize-exclusion set, and the rule
/// kind. In production these are owned by the analysis run and shared across targets; a
/// test owns them here and lends a fully-wired context via [`context`](Self::context),
/// so a test mints exactly what the analyzer would in one call (rather than threading
/// four borrows through every `RuleContext::new`). Add an executor for `query` by
/// chaining [`RuleContext::with_executor`] on the result.
pub struct TestContext {
    source_paths: SourcePathRecorder,
    state_registry: StateRegistry,
    materialized: BTreeSet<PathBuf>,
    rule_kind: String,
}

impl Default for TestContext {
    fn default() -> Self {
        TestContext {
            source_paths: SourcePathRecorder::default(),
            state_registry: StateRegistry::new(),
            materialized: BTreeSet::new(),
            rule_kind: "test".to_owned(),
        }
    }
}

impl TestContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the declaring rule's kind (default `"test"`) — affects state keys (§2.6).
    pub fn rule_kind(mut self, kind: impl Into<String>) -> Self {
        self.rule_kind = kind.into();
        self
    }

    /// Set the materialize-exclusion set (default empty).
    pub fn materialized(mut self, paths: BTreeSet<PathBuf>) -> Self {
        self.materialized = paths;
        self
    }

    /// The recorder, to assert which source paths a rule read.
    pub fn source_paths(&self) -> &SourcePathRecorder {
        &self.source_paths
    }

    /// Lend a fully-wired context for one target's facts.
    pub fn context<'a>(
        &'a self,
        label: Label,
        attrs: &'a Attrs,
        config: &'a Configuration,
        package_dir: &'a Path,
        cas: &'a Cas,
        deps: &'a [ResolvedDep],
    ) -> RuleContext<'a> {
        RuleContext::new(
            label,
            attrs,
            config,
            package_dir,
            cas,
            deps,
            &self.source_paths,
            &self.rule_kind,
            &self.state_registry,
            &self.materialized,
        )
    }
}

fn workspace_relative_path(package: &str, rel: &Path) -> PathBuf {
    let mut path = PathBuf::new();
    if !package.is_empty() {
        path.push(package);
    }
    if rel != Path::new(".") {
        path.push(rel);
    }
    path
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attrs::Attrs;
    use anneal_core::{AxisValues, Platform};

    /// `source_tree` skips materialized tree copies: they are generated
    /// outputs parked in the tree for native tools, not sources — recording
    /// them would shadow the producing action's declared output.
    #[test]
    fn source_tree_skips_materialized_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("real.txt"), b"source").unwrap();
        std::fs::write(pkg.join("config.json"), b"generated").unwrap();
        let cas = Cas::open(tmp.path().join("cas")).unwrap();
        let attrs = Attrs::builder().build();
        let config = Configuration::new(Platform::new("host", "host"), AxisValues::default());
        let label = Label::parse("//pkg:t").unwrap();
        let deps: Vec<ResolvedDep> = Vec::new();

        // The exclusion set holds workspace-relative paths.
        let materialized: BTreeSet<PathBuf> = [PathBuf::from("pkg/config.json")].into();
        let tc = TestContext::new().materialized(materialized);
        let ctx = tc.context(label.clone(), &attrs, &config, &pkg, &cas, &deps);

        let artifacts = ctx.source_tree(Path::new("."), &[]).unwrap();
        let paths: Vec<&Path> = artifacts.iter().map(|a| a.path.as_path()).collect();
        assert_eq!(paths, vec![Path::new("real.txt")]);
        assert!(
            !tc.source_paths()
                .paths()
                .contains(Path::new("pkg/config.json")),
            "a materialized copy must not be recorded as a source"
        );

        // Without the exclusion the same file is an ordinary source.
        let tc = TestContext::new();
        let ctx = tc.context(label, &attrs, &config, &pkg, &cas, &deps);
        let artifacts = ctx.source_tree(Path::new("."), &[]).unwrap();
        assert_eq!(artifacts.len(), 2);
    }
}
