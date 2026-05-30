//! The analyzer: dependency-ordered rule analysis with provider threading and
//! memoization, producing an [`ActionGraph`].

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anneal_cas::Cas;
use anneal_core::{Configuration, Label};
use anneal_exec::Action;
use anneal_loader::TargetGraph;
use anneal_rules::{ProviderSet, ResolvedDep, RuleContext, RuleRegistry};

use crate::error::AnalysisError;

/// The result of analyzing one configured target.
#[derive(Debug, Clone)]
pub struct AnalyzedTarget {
    pub label: Label,
    /// Providers exposed to dependents.
    pub providers: ProviderSet,
    /// Actions contributed by this target (often empty for provider-only rules).
    pub actions: Vec<Action>,
}

/// The analyzed action graph: every reached target's result, plus a dependency
/// (topological) ordering — dependencies precede their dependents.
#[derive(Debug)]
pub struct ActionGraph {
    targets: HashMap<Label, AnalyzedTarget>,
    order: Vec<Label>,
}

impl ActionGraph {
    /// All actions, in dependency order (a producer's actions precede its
    /// consumers').
    pub fn actions(&self) -> impl Iterator<Item = &Action> + '_ {
        self.order
            .iter()
            .flat_map(move |label| self.targets[label].actions.iter())
    }

    /// Total number of actions in the graph.
    pub fn action_count(&self) -> usize {
        self.order
            .iter()
            .map(|label| self.targets[label].actions.len())
            .sum()
    }

    /// The analyzed result for a target.
    pub fn target(&self, label: &Label) -> Option<&AnalyzedTarget> {
        self.targets.get(label)
    }

    /// The providers a target exposed.
    pub fn providers(&self, label: &Label) -> Option<&ProviderSet> {
        self.targets.get(label).map(|t| &t.providers)
    }

    /// The targets in dependency order.
    pub fn order(&self) -> &[Label] {
        &self.order
    }
}

/// Analyzes targets from a [`TargetGraph`] under a fixed [`Configuration`].
pub struct Analyzer<'a> {
    graph: &'a TargetGraph,
    registry: &'a RuleRegistry,
    config: &'a Configuration,
    workspace_root: &'a Path,
    cas: &'a Cas,
}

impl<'a> Analyzer<'a> {
    pub fn new(
        graph: &'a TargetGraph,
        registry: &'a RuleRegistry,
        config: &'a Configuration,
        workspace_root: &'a Path,
        cas: &'a Cas,
    ) -> Self {
        Analyzer {
            graph,
            registry,
            config,
            workspace_root,
            cas,
        }
    }

    /// Analyze `root` and its transitive dependencies into an [`ActionGraph`].
    pub fn analyze(&self, root: &Label) -> Result<ActionGraph, AnalysisError> {
        let mut targets = HashMap::new();
        let mut order = Vec::new();
        let mut in_progress = HashSet::new();
        self.visit(root, &mut targets, &mut order, &mut in_progress)?;
        Ok(ActionGraph { targets, order })
    }

    fn visit(
        &self,
        label: &Label,
        targets: &mut HashMap<Label, AnalyzedTarget>,
        order: &mut Vec<Label>,
        in_progress: &mut HashSet<Label>,
    ) -> Result<(), AnalysisError> {
        if targets.contains_key(label) {
            return Ok(()); // memoized — diamond dependencies analyzed once
        }
        if !in_progress.insert(label.clone()) {
            return Err(AnalysisError::Cycle(label.clone()));
        }

        let decl = self
            .graph
            .get(label)
            .ok_or_else(|| AnalysisError::MissingTarget(label.clone()))?;

        // Analyze dependencies first so their providers are available.
        for dep in &decl.deps {
            self.visit(dep, targets, order, in_progress)?;
        }

        // Thread each dependency's providers into this target's context.
        let resolved_deps: Vec<ResolvedDep> = decl
            .deps
            .iter()
            .map(|dep| ResolvedDep {
                label: dep.clone(),
                providers: targets[dep].providers.clone(),
            })
            .collect();

        let rule = self
            .registry
            .get(&decl.kind)
            .ok_or_else(|| AnalysisError::UnknownRule {
                label: label.clone(),
                kind: decl.kind.clone(),
            })?;

        let package_dir = self.workspace_root.join(decl.label.package());
        let ctx = RuleContext::new(
            decl.label.clone(),
            &decl.attrs,
            self.config,
            &package_dir,
            self.cas,
            &resolved_deps,
        );
        let analysis = rule.analyze(&ctx).map_err(|error| AnalysisError::Rule {
            label: label.clone(),
            error,
        })?;

        in_progress.remove(label);
        order.push(label.clone());
        targets.insert(
            label.clone(),
            AnalyzedTarget {
                label: label.clone(),
                providers: analysis.providers,
                actions: analysis.actions,
            },
        );
        Ok(())
    }
}
