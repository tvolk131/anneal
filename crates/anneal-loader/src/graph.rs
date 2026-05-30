//! The target graph: the loader's output (§3.1, §3.2).

use std::collections::BTreeMap;

use anneal_core::Label;
use anneal_rules::Attrs;

/// One target as declared in a `BUILD` file: a rule instance with validated
/// attributes and its declared dependency edges.
#[derive(Debug, Clone)]
pub struct TargetDecl {
    pub label: Label,
    /// Rule kind, e.g. `"genrule"`.
    pub kind: String,
    /// Validated, typed attributes.
    pub attrs: Attrs,
    /// Dependency labels extracted from this target's label-typed attributes.
    pub deps: Vec<Label>,
    /// Source location (`file:line:col`) of the rule call, for diagnostics.
    pub location: Option<String>,
}

/// A graph of declared targets, keyed by label. Edges are the `deps` of each
/// [`TargetDecl`]. (Package-level: roughly one node per package's worth of targets.)
#[derive(Debug, Default)]
pub struct TargetGraph {
    targets: BTreeMap<Label, TargetDecl>,
}

impl TargetGraph {
    /// Look up a target by label.
    pub fn get(&self, label: &Label) -> Option<&TargetDecl> {
        self.targets.get(label)
    }

    /// Iterate all targets, in label order.
    pub fn targets(&self) -> impl Iterator<Item = &TargetDecl> + '_ {
        self.targets.values()
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub(crate) fn insert(&mut self, decl: TargetDecl) {
        self.targets.insert(decl.label.clone(), decl);
    }
}
