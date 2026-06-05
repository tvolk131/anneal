//! The analyzer: dependency-ordered rule analysis with provider threading and
//! memoization, producing an [`ActionGraph`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anneal_cas::Cas;
use anneal_core::{Configuration, Label};
use anneal_exec::Action;
use anneal_loader::TargetGraph;
use anneal_rules::{ProviderSet, ResolvedDep, RuleContext, RuleRegistry, SourcePathRecorder};

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
        let source_paths = SourcePathRecorder::default();
        self.visit(
            root,
            &mut targets,
            &mut order,
            &mut in_progress,
            &source_paths,
        )?;
        let graph = ActionGraph { targets, order };
        graph.validate_generated_paths(&source_paths.paths())?;
        Ok(graph)
    }

    fn visit(
        &self,
        label: &Label,
        targets: &mut HashMap<Label, AnalyzedTarget>,
        order: &mut Vec<Label>,
        in_progress: &mut HashSet<Label>,
        source_paths: &SourcePathRecorder,
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
            self.visit(dep, targets, order, in_progress, source_paths)?;
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
        source_paths.record_workspace_path(workspace_relative_path(
            decl.label.package(),
            Path::new("BUILD"),
        ));
        let ctx = RuleContext::new_recording_sources(
            decl.label.clone(),
            &decl.attrs,
            self.config,
            &package_dir,
            self.cas,
            &resolved_deps,
            source_paths,
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

#[derive(Debug, Clone)]
struct OutputOwner {
    label: Label,
    action: String,
    output: String,
}

impl ActionGraph {
    fn validate_generated_paths(
        &self,
        source_paths: &BTreeSet<PathBuf>,
    ) -> Result<(), AnalysisError> {
        let mut generated: BTreeMap<PathBuf, OutputOwner> = BTreeMap::new();
        for label in &self.order {
            let target = &self.targets[label];
            for action in &target.actions {
                for (output, path) in action.outputs() {
                    let workspace_path =
                        action_workspace_path(&target.label, action, path.as_path());
                    if source_paths.contains(&workspace_path) {
                        return Err(AnalysisError::GeneratedOutputShadowsSource {
                            path: workspace_path,
                            label: target.label.clone(),
                            action: action.name().to_owned(),
                            output: output.clone(),
                        });
                    }

                    let owner = OutputOwner {
                        label: target.label.clone(),
                        action: action.name().to_owned(),
                        output: output.clone(),
                    };
                    if let Some(first) = generated.insert(workspace_path.clone(), owner.clone()) {
                        return Err(AnalysisError::GeneratedOutputCollision {
                            path: workspace_path,
                            first_label: first.label,
                            first_action: first.action,
                            first_output: first.output,
                            second_label: owner.label,
                            second_action: owner.action,
                            second_output: owner.output,
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

fn action_workspace_path(label: &Label, action: &Action, path: &Path) -> PathBuf {
    let mut workspace_path = workspace_relative_path(label.package(), action.working_directory());
    workspace_path.push(path);
    workspace_path
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
