//! The analysis context the system hands to a rule.
//!
//! [`RuleContext`] is the rule's entire view of the world: its label, typed
//! attributes, configuration, the providers of its already-analyzed dependencies,
//! and a source-file resolver. A rule cannot reach outside this — it can't read
//! arbitrary files or inspect global state — which keeps the system/rule boundary
//! sharp.

use std::path::Path;

use anneal_cas::Cas;
use anneal_core::{Configuration, Label};

use crate::attrs::Attrs;
use crate::providers::{Artifact, ProviderSet};
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
        let abs = self.package_dir.join(rel);
        let bytes = std::fs::read(&abs).map_err(|error| RuleError::Source {
            path: rel.to_path_buf(),
            error,
        })?;
        let digest = self.cas.put(&bytes).map_err(|error| RuleError::Source {
            path: rel.to_path_buf(),
            error,
        })?;
        Ok(Artifact {
            path: rel.to_path_buf(),
            digest,
        })
    }
}
