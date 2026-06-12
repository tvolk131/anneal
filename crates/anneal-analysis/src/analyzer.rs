//! The analyzer: dependency-ordered rule analysis with provider threading and
//! memoization, producing an [`ActionGraph`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anneal_cas::Cas;
use anneal_core::{Configuration, ExecMode, Label};
use anneal_exec::{Action, LocalExecutor};
use anneal_loader::TargetGraph;
use anneal_rules::{
    Artifact, ProviderSet, ResolvedDep, RuleContext, RuleRegistry, SourcePathRecorder,
    StateRegistry,
};

use crate::error::AnalysisError;

/// The result of analyzing one configured target.
#[derive(Debug, Clone)]
pub struct AnalyzedTarget {
    pub label: Label,
    /// The configuration this target was analyzed under — its configured
    /// identity. Differs across nodes when the focus cone colors the graph.
    pub config: Configuration,
    /// Providers exposed to dependents.
    pub providers: ProviderSet,
    /// Actions contributed by this target (often empty for provider-only rules).
    pub actions: Vec<Action>,
    /// The generated files this target's actions consume at tree-shaped paths
    /// (the rule's `Analysis::routed_data`), with each `path` re-homed to a
    /// **workspace-relative** destination — what `anneal materialize` parks.
    pub routed_data: Vec<Artifact>,
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

    /// The generated files a target's build routes into its package tree
    /// (workspace-relative destinations) — the set `anneal materialize` parks
    /// so native tools see what the sandbox sees.
    pub fn routed_data(&self, label: &Label) -> Option<&[Artifact]> {
        self.targets.get(label).map(|t| t.routed_data.as_slice())
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
    /// Cross-target persistent-state declarations for this run (idempotence +
    /// mismatch checking; DESIGN.md §3.3).
    states: StateRegistry,
    /// Executor for analysis-time tool queries (DESIGN.md §3.6). Optional:
    /// rules that never query analyze fine without one.
    executor: Option<&'a LocalExecutor>,
    /// The focus cone (DESIGN.md §4.2): labels colored `Incremental` — the
    /// edited targets plus their transitive dependents. `None` means uniform
    /// coloring (every node gets the base configuration unchanged); `Some`
    /// means per-node coloring: cone members build Incremental, everything
    /// else Hermetic. One configuration per node per invocation, always.
    cone: Option<HashSet<Label>>,
    /// Workspace-relative tree paths written by `anneal materialize`, excluded
    /// from every rule's source discovery (they are parked copies of generated
    /// outputs, not sources — see `RuleContext::with_materialized`).
    materialized: BTreeSet<PathBuf>,
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
            states: StateRegistry::new(),
            executor: None,
            cone: None,
            materialized: BTreeSet::new(),
        }
    }

    /// Exclude `anneal materialize`-written tree paths (workspace-relative,
    /// from the materialize manifest) from source discovery. Without this, a
    /// materialized copy would be recorded as a source and collide with the
    /// producing action's declared output in `validate_generated_paths`.
    pub fn with_materialized_paths(mut self, paths: BTreeSet<PathBuf>) -> Self {
        self.materialized = paths;
        self
    }

    /// Color the graph per node (DESIGN.md §4.2): labels in `cone` analyze
    /// under `ExecMode::Incremental`, all others under `ExecMode::Hermetic`
    /// (the base configuration's other axes are shared by every node). Without
    /// this call, every node gets the base configuration unchanged — the
    /// uniform coloring the `--exec-mode` flag forces.
    pub fn with_incremental_cone(mut self, cone: HashSet<Label>) -> Self {
        self.cone = Some(cone);
        self
    }

    /// The configuration a node analyzes under.
    fn node_config(&self, label: &Label) -> Configuration {
        match &self.cone {
            None => self.config.clone(),
            Some(cone) => {
                let mut axes = self.config.axes().clone();
                axes.exec_mode = if cone.contains(label) {
                    ExecMode::Incremental
                } else {
                    ExecMode::Hermetic
                };
                Configuration::new(self.config.platform().clone(), axes)
            }
        }
    }

    /// The §4.3 monotonicity assert, at edge-resolution time: no Hermetic node
    /// may depend on an Incremental node. By construction of the cone (edited
    /// targets plus transitive *dependents*) this cannot fire — which is
    /// exactly why it is asserted rather than trusted: a future coloring-policy
    /// tweak (or a pin flag that fails to take the monotone closure) would
    /// otherwise silently poison the shared cache.
    fn assert_monotone(&self, node: &Label, dep: &Label) -> Result<(), AnalysisError> {
        let Some(cone) = &self.cone else {
            return Ok(());
        };
        if !cone.contains(node) && cone.contains(dep) {
            return Err(AnalysisError::ConeViolation {
                hermetic: node.clone(),
                incremental: dep.clone(),
            });
        }
        Ok(())
    }

    /// Enable analysis-time tool queries by wiring the executor through to
    /// rule contexts. Queries are sealed, network-denied, stdout-captured
    /// actions — this is the §5.1 by-design breach of strict phasing.
    pub fn with_executor(mut self, executor: &'a LocalExecutor) -> Self {
        self.executor = Some(executor);
        self
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
            self.assert_monotone(label, dep)?;
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
        let node_config = self.node_config(label);
        let ctx = RuleContext::new_recording_sources(
            decl.label.clone(),
            &decl.attrs,
            &node_config,
            &package_dir,
            self.cas,
            &resolved_deps,
            source_paths,
        )
        .with_rule_kind(rule.kind())
        .with_state_registry(&self.states);
        let ctx = match self.executor {
            Some(executor) => ctx.with_executor(executor),
            None => ctx,
        };
        let ctx = if self.materialized.is_empty() {
            ctx
        } else {
            ctx.with_materialized(&self.materialized)
        };
        let analysis = rule.analyze(&ctx).map_err(|error| AnalysisError::Rule {
            label: label.clone(),
            error,
        })?;

        in_progress.remove(label);
        order.push(label.clone());
        // Rules declare routed destinations package-relative (all a rule can
        // see); re-home them to workspace-relative here, where the package is
        // known — so consumers of the graph never re-derive it.
        let routed_data = analysis
            .routed_data
            .into_iter()
            .map(|artifact| Artifact {
                path: workspace_relative_path(decl.label.package(), &artifact.path),
                source: artifact.source,
            })
            .collect();
        targets.insert(
            label.clone(),
            AnalyzedTarget {
                label: label.clone(),
                config: node_config,
                providers: analysis.providers,
                actions: analysis.actions,
                routed_data,
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
