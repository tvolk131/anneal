//! `anneal` — the Anneal command-line interface (§18).
//!
//! Deliberately a **thin coordinator** (the plan's crate decomposition allows this one
//! crate to be a coordinator): it parses flags, builds a [`Configuration`] from the
//! universal-axis selectors (§6.6), and drives the existing pipeline —
//! `load_package → Analyzer → LocalExecutor::execute_graph` — then reports.
//!
//! # Milestone-1 scope
//!
//! Two commands, `build` and `test`, over a **single package** (the one named by the
//! target label) — multi-package workspace loading and the `query`/`affected`/`why`
//! commands are the next increment (§11.3). All logic lives in the libraries; this file
//! only orchestrates and formats.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anneal_analysis::{ActionGraph, Analyzer};
use anneal_core::{
    AxisValues, Configuration, Coverage, DebugInfo, Digest, ExecMode, Label, Lto, OptLevel,
    Platform, Sanitizer,
};
use anneal_exec::materialize::{MaterializeStore, TreeState};
use anneal_exec::{Action, ActionResult, LocalExecutor};
use anneal_loader::{load_closure, load_workspace};
use anneal_rules::{builtin_rules, ArtifactSource};
use clap::{Args, Parser, Subcommand};

mod lock;

/// Anneal — a native-tool-preserving build system.
#[derive(Parser)]
#[command(name = "anneal", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    #[command(flatten)]
    config: ConfigArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Build a target: analyze it and run its action graph.
    Build {
        /// The target label, e.g. `//app:app`.
        target: String,
    },
    /// Build a target and summarize its test results.
    Test {
        /// The target label, e.g. `//app:app`.
        target: String,
    },
    /// List the targets affected by changes since a git ref (§11.3).
    Affected {
        /// The git ref to diff against (e.g. `origin/main`, a commit SHA).
        #[arg(long)]
        since: String,
    },
    /// Build a target and write its provided files into the working tree, so
    /// native tools (cargo run, rust-analyzer) can read generated inputs. Tree
    /// copies are written read-only, tracked in `.anneal/materialized`, and
    /// ignored by source discovery — the routed action edge remains the
    /// build's real input. Inverse: `--clean`.
    Materialize {
        /// The producing target label, e.g. `//:config`. Optional with
        /// `--clean` / `--list`.
        target: Option<String>,
        /// Report fresh/stale instead of writing; exit 1 if anything is stale.
        #[arg(long, conflicts_with_all = ["clean", "list"])]
        check: bool,
        /// Remove materialized files (all of them, or only TARGET's).
        #[arg(long, conflicts_with = "list")]
        clean: bool,
        /// List materialized files and whether each is intact/edited/missing.
        #[arg(long)]
        list: bool,
        /// Overwrite or remove files even if edited since materialization (and
        /// allow git-tracked destinations).
        #[arg(long)]
        force: bool,
    },
    /// Explain a dependency relationship: the shortest path from one target to
    /// another (`why <from> <to>`), or why a target is affected by a change
    /// (`why <from> --since <ref>`).
    Why {
        /// The target to explain.
        from: String,
        /// Show the dependency path to this target.
        to: Option<String>,
        /// Instead, explain why `from` is affected by changes since this ref.
        #[arg(long, conflicts_with = "to")]
        since: Option<String>,
    },
}

/// The universal configuration axes (§6.2, §6.6), selectable per invocation. Each is
/// global so it can appear before or after the subcommand.
#[derive(Args)]
struct ConfigArgs {
    /// Workspace root (defaults to the current directory).
    #[arg(long, global = true, value_name = "DIR")]
    workspace_root: Option<PathBuf>,
    /// Target platform triple (§6.6 `--target`). Defaults to a host placeholder.
    #[arg(long, global = true, value_name = "TRIPLE")]
    platform: Option<String>,
    /// `debug` | `release` | `release-with-debuginfo`.
    #[arg(long, global = true, value_name = "LEVEL")]
    opt_level: Option<String>,
    /// `off` | `thin` | `full`.
    #[arg(long, global = true, value_name = "MODE")]
    lto: Option<String>,
    /// `none` | `line-tables-only` | `full`.
    #[arg(long, global = true, value_name = "LEVEL")]
    debug_info: Option<String>,
    /// `none` | `address` | `thread` | `memory` | `undefined`.
    #[arg(long, global = true, value_name = "KIND")]
    sanitizer: Option<String>,
    /// `on` | `off`.
    #[arg(long, global = true, value_name = "STATE")]
    coverage: Option<String>,
    /// `incremental` | `hermetic` (DESIGN.md §4.1). Incremental actions may use
    /// warm interleaved tool state (fast, machine-local results); hermetic
    /// actions may not (cold, deterministic, shareable). Default: incremental —
    /// the dev loop. CI passes `--exec-mode hermetic` (with `--require-enforced`).
    #[arg(long, global = true, value_name = "MODE")]
    exec_mode: Option<String>,
    /// Max actions to run concurrently. Defaults to the machine's parallelism.
    /// Scheduling-only — it never affects cache keys or results.
    #[arg(long, global = true, value_name = "N")]
    jobs: Option<usize>,
    /// Fail sealed execution on any platform whose sandbox enforcement is below
    /// `enforced` (Linux namespaces), instead of silently degrading — the
    /// mandatory CI posture (DESIGN.md §2.8). macOS Seatbelt is `loud-best-effort`,
    /// so this flag fails sealed actions on macOS by design.
    #[arg(long, global = true)]
    require_enforced: bool,
}

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            2
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32, String> {
    let root = match &cli.config.workspace_root {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().map_err(|e| format!("cannot read current dir: {e}"))?,
    };
    let config = build_config(&cli.config)?;
    match cli.command {
        Command::Build { target } => build(
            &target,
            &config,
            &root,
            cli.config.jobs,
            cli.config.require_enforced,
            cli.config.exec_mode.is_none(),
        ),
        Command::Test { target } => test(
            &target,
            &config,
            &root,
            cli.config.jobs,
            cli.config.require_enforced,
            cli.config.exec_mode.is_none(),
        ),
        Command::Materialize {
            target,
            check,
            clean,
            list,
            force,
        } => {
            if list {
                materialize_list(&root)
            } else if clean {
                materialize_clean(target.as_deref(), force, &root)
            } else {
                let target = target.as_deref().ok_or_else(|| {
                    "materialize requires a target label (or --clean / --list)".to_owned()
                })?;
                materialize(
                    target,
                    check,
                    force,
                    &config,
                    &root,
                    cli.config.jobs,
                    cli.config.require_enforced,
                    cli.config.exec_mode.is_none(),
                )
            }
        }
        Command::Affected { since } => affected(&since, &root),
        Command::Why { from, to, since } => why(&from, to.as_deref(), since.as_deref(), &root),
    }
}

/// Explain a dependency relationship (§11.3). With `<to>`: the shortest path from `from`
/// to `to`. With `--since`: why `from` is affected by changes since the ref (the path to
/// the nearest changed target in `from`'s dependency closure). Uses `from`'s forward
/// closure only — no whole-workspace load.
fn why(from: &str, to: Option<&str>, since: Option<&str>, root: &Path) -> Result<i32, String> {
    let from_label = Label::parse(from).map_err(|e| format!("invalid target {from:?}: {e}"))?;
    let registry = builtin_rules();
    let graph = load_closure(root, &from_label, &registry).map_err(|e| e.to_string())?;

    match (to, since) {
        (Some(to), _) => {
            let to_label = Label::parse(to).map_err(|e| format!("invalid target {to:?}: {e}"))?;
            match anneal_query::why(&graph, &from_label, &to_label) {
                Some(path) => print_path(&path),
                None => println!("no path from {from_label} to {to_label}"),
            }
            Ok(0)
        }
        (None, Some(since)) => why_affected(&graph, &from_label, since, root),
        (None, None) => Err("specify a <to> target or --since <ref>".to_owned()),
    }
}

/// `why <from> --since <ref>`: the path from `from` to the nearest changed target.
fn why_affected(
    graph: &anneal_loader::TargetGraph,
    from: &Label,
    since: &str,
    root: &Path,
) -> Result<i32, String> {
    let changed = git_changed_files(root, since)?;
    let mut changed_packages: BTreeSet<String> = BTreeSet::new();
    let mut unowned: Vec<PathBuf> = Vec::new();
    for path in &changed {
        match anneal_query::owner(root, path) {
            Some(pkg) => {
                changed_packages.insert(pkg);
            }
            None => unowned.push(path.clone()),
        }
    }
    if !unowned.is_empty() {
        println!(
            "{from} is affected: an unowned file changed (e.g. {}), so the whole workspace is \
             conservatively affected",
            unowned[0].display()
        );
        return Ok(0);
    }

    // The changed targets within `from`'s dependency closure.
    let changed_targets: BTreeSet<Label> = graph
        .targets()
        .filter(|t| changed_packages.contains(t.label.package()))
        .map(|t| t.label.clone())
        .collect();

    match anneal_query::shortest_path(graph, from, &changed_targets) {
        Some(path) => {
            println!("{from} is affected by changes since {since}:");
            print_path(&path);
            Ok(0)
        }
        None => {
            println!("{from} is not affected by changes since {since}");
            Ok(0)
        }
    }
}

/// Render a dependency path as `a → b → c`.
fn print_path(path: &[Label]) {
    let rendered: Vec<String> = path.iter().map(|l| l.to_string()).collect();
    println!("  {}", rendered.join(" → "));
}

/// Print the targets affected by changes since `since` (§11.3): `git diff` → owning
/// packages → reverse-dependency closure. Loads the whole workspace (reverse-deps need
/// every target), but runs no analysis.
fn affected(since: &str, root: &Path) -> Result<i32, String> {
    let changed = git_changed_files(root, since)?;
    if changed.is_empty() {
        println!("no changes since {since}");
        return Ok(0);
    }
    let graph = load_workspace(root, &builtin_rules()).map_err(|e| e.to_string())?;
    let result = anneal_query::affected(root, &graph, &changed);

    if result.workspace_wide {
        eprintln!(
            "note: {} change(s) outside any package (e.g. {}) — treating the whole workspace as affected",
            result.unowned.len(),
            result.unowned[0].display(),
        );
    }
    for label in &result.targets {
        println!("{label}");
    }
    Ok(0)
}

/// The focus cone for the demanded graph: the labels whose packages own dirty
/// files, plus their transitive dependents (`anneal_query::affected` is exactly
/// this computation). `None` means "color everything Incremental": not a git
/// repo, or dirty files outside any package (workspace-wide edits) — in every
/// case mis-coloring is a performance question only (DESIGN.md §4.2).
fn incremental_cone(
    root: &Path,
    graph: &anneal_loader::TargetGraph,
) -> Option<std::collections::HashSet<Label>> {
    let dirty = git_dirty_files(root).ok()?;
    if dirty.is_empty() {
        // Clean tree: nothing is being edited, so the whole graph is
        // Hermetic-eligible — an empty cone is correct, not a fallback.
        return Some(std::collections::HashSet::new());
    }
    let result = anneal_query::affected(root, graph, &dirty);
    if result.workspace_wide {
        return None;
    }
    Some(result.targets.into_iter().collect())
}

/// The dirty working tree (staged, unstaged, and untracked), as
/// workspace-relative paths — the v1 edit horizon for the focus cone.
fn git_dirty_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let out = ProcessCommand::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git status --porcelain failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(parse_porcelain(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `git status --porcelain` output into paths. Rename lines
/// (`R  old -> new`) contribute both sides — the old path's owner is affected
/// by the removal, the new path's by the addition.
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

/// Files changed in the working tree relative to `since` (workspace-root == git-root).
/// Untracked-but-unadded files are not reported by `git diff` — a known limitation.
fn git_changed_files(root: &Path, since: &str) -> Result<Vec<PathBuf>, String> {
    let out = ProcessCommand::new("git")
        .args(["diff", "--name-only", since])
        .current_dir(root)
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git diff --name-only {since} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
}

/// Analyze and execute a target's action graph; return the process exit code.
fn build(
    target: &str,
    config: &Configuration,
    root: &Path,
    jobs: Option<usize>,
    require_enforced: bool,
    auto_cone: bool,
) -> Result<i32, String> {
    let run = analyze_and_run(target, config, root, jobs, require_enforced, auto_cone)?;
    report_actions(&run.actions, &run.results);

    let failed = run.results.iter().filter(|r| !r.success()).count();
    let cached = run.results.iter().filter(|r| r.cache_hit).count();
    if failed > 0 {
        eprintln!(
            "build FAILED — {failed}/{} action(s) failed",
            run.actions.len()
        );
        Ok(1)
    } else {
        println!(
            "build ok — {} action(s) ({cached} cached)",
            run.actions.len()
        );
        Ok(0)
    }
}

/// Build, then summarize the actions that produced a test result (`results.txt`).
fn test(
    target: &str,
    config: &Configuration,
    root: &Path,
    jobs: Option<usize>,
    require_enforced: bool,
    auto_cone: bool,
) -> Result<i32, String> {
    let run = analyze_and_run(target, config, root, jobs, require_enforced, auto_cone)?;
    report_actions(&run.actions, &run.results);

    // Test actions are rule-agnostic: any action that captured `results.txt` and wrote
    // the `ANNEAL_TEST_EXIT` marker (cargo's test-run, pnpm's test kind). Structured
    // per-case parsing is a later increment; here we report pass/fail per test action.
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut saw_test = false;
    for (action, result) in run.actions.iter().zip(&run.results) {
        if let Some(ok) = test_outcome(&run.exec, result) {
            saw_test = true;
            if ok {
                passed += 1;
            } else {
                failed += 1;
                println!("  FAIL  {}", action.name());
            }
        }
    }

    // A non-zero action exit (a build/compile failure) also fails the run.
    let action_failures = run.results.iter().filter(|r| !r.success()).count();
    if !saw_test && action_failures == 0 {
        println!("no test targets found for {target}");
        return Ok(0);
    }
    println!("tests: {passed} passed, {failed} failed");
    if failed > 0 || action_failures > 0 {
        Ok(1)
    } else {
        Ok(0)
    }
}

/// The shared pipeline's result: everything a command needs after execution.
/// Holds the workspace lock so a mutating command stays exclusive for its
/// whole run — including post-execution tree writes (`materialize`).
struct PipelineRun {
    graph: ActionGraph,
    actions: Vec<Action>,
    results: Vec<ActionResult>,
    exec: LocalExecutor,
    _lock: lock::WorkspaceLock,
}

/// The shared pipeline: parse the label, load its package, analyze, execute the graph.
fn analyze_and_run(
    target: &str,
    config: &Configuration,
    root: &Path,
    jobs: Option<usize>,
    require_enforced: bool,
    auto_cone: bool,
) -> Result<PipelineRun, String> {
    let label = Label::parse(target).map_err(|e| format!("invalid target {target:?}: {e}"))?;
    let registry = builtin_rules();
    // A mutating command takes the coarse exclusive workspace lock for its whole run, so
    // concurrent `anneal` processes can't collide on shared warm dirs / sandboxes. Held
    // until the returned `PipelineRun` is dropped. Read-only commands
    // (`affected`/`why`) deliberately do not acquire it. (See lock.rs.)
    let lock = lock::WorkspaceLock::acquire(&root.join(".anneal"))
        .map_err(|e| format!("acquiring workspace lock: {e}"))?;
    // Load the target's transitive package closure (cross-package deps included).
    let graph = load_closure(root, &label, &registry).map_err(|e| e.to_string())?;
    let exec = LocalExecutor::new(root.join(".anneal"))
        .map_err(|e| format!("opening .anneal store: {e}"))?;
    let exec = match jobs {
        Some(j) => exec.jobs(j),
        None => exec,
    };
    let exec = exec.require_enforced(require_enforced);
    // Tree copies written by `anneal materialize` are not sources: exclude
    // them from every rule's source discovery (otherwise they'd shadow the
    // producing action's declared output — an analysis-time hard error — and
    // perturb source-derived snapshot keys).
    let materialized = MaterializeStore::open(root.join(".anneal"), root)
        .map_err(|e| format!("reading materialize manifest: {e}"))?
        .paths();
    let analyzer = Analyzer::new(&graph, &registry, config, root, exec.cas())
        .with_executor(&exec)
        .with_materialized_paths(materialized);
    // Default coloring (DESIGN.md §4.2): the focus cone. Edited targets (the
    // dirty working tree) plus their transitive dependents build Incremental;
    // everything upstream builds Hermetic, where unchanged inputs are pure
    // cache hits. `--exec-mode` forces a uniform mode instead. Fallbacks are
    // performance-conservative, never soundness-relevant: no git, or changes
    // outside any package, color everything Incremental (today's behavior).
    let analyzer = if auto_cone {
        match incremental_cone(root, &graph) {
            Some(cone) => {
                let total = graph.len();
                println!(
                    "focus cone: {} incremental / {} hermetic target(s)",
                    cone.len(),
                    total.saturating_sub(cone.len())
                );
                analyzer.with_incremental_cone(cone)
            }
            None => analyzer,
        }
    } else {
        analyzer
    };
    let analyzed = analyzer.analyze(&label).map_err(|e| e.to_string())?;
    let actions: Vec<Action> = analyzed.actions().cloned().collect();
    let results = exec.execute_graph(&actions).map_err(|e| e.to_string())?;
    Ok(PipelineRun {
        graph: analyzed,
        actions,
        results,
        exec,
        _lock: lock,
    })
}

/// `materialize <target>`: build it (cached), then park its provided files in
/// the working tree so native tools see generated inputs. The build's real
/// input stays the routed action edge — `analyze_and_run` excludes the tree
/// copies from source discovery via the manifest.
#[allow(clippy::too_many_arguments)]
fn materialize(
    target: &str,
    check: bool,
    force: bool,
    config: &Configuration,
    root: &Path,
    jobs: Option<usize>,
    require_enforced: bool,
    auto_cone: bool,
) -> Result<i32, String> {
    let label = Label::parse(target).map_err(|e| format!("invalid target {target:?}: {e}"))?;
    let run = analyze_and_run(target, config, root, jobs, require_enforced, auto_cone)?;

    let failed = run.results.iter().filter(|r| !r.success()).count();
    if failed > 0 {
        report_actions(&run.actions, &run.results);
        eprintln!(
            "build FAILED — {failed}/{} action(s) failed; not materializing",
            run.actions.len()
        );
        return Ok(1);
    }

    let files = provided_files(&run, &label)?;
    if files.is_empty() {
        println!("{label} provides no files to materialize");
        return Ok(0);
    }

    let open_store = || {
        MaterializeStore::open(root.join(".anneal"), root)
            .map_err(|e| format!("reading materialize manifest: {e}"))
    };

    if check {
        let report = open_store()?
            .check(&label.to_string(), &files)
            .map_err(|e| format!("checking materialized files: {e}"))?;
        for path in &report.fresh {
            println!("   fresh  {}", path.display());
        }
        for path in &report.stale {
            println!("   STALE  {}", path.display());
        }
        return Ok(if report.stale.is_empty() { 0 } else { 1 });
    }

    // Refuse git-tracked destinations: a *generated* file that is also
    // committed is a conflict for the user to resolve, not overwrite.
    if !force {
        let tracked = git_tracked(root, files.iter().map(|(path, _)| path.as_path()));
        if !tracked.is_empty() {
            return Err(format!(
                "destination(s) tracked in git: {} — generated files should be gitignored; \
                 pass --force to overwrite anyway",
                join_paths(&tracked)
            ));
        }
    }

    let report = open_store()?
        .apply(&label.to_string(), &files, run.exec.cas(), force)
        .map_err(|e| format!("materializing: {e}"))?;
    for path in &report.written {
        println!("   wrote      {}", path.display());
    }
    for path in &report.unchanged {
        println!("   unchanged  {}", path.display());
    }
    for path in &report.pruned {
        println!("   pruned     {}", path.display());
    }
    for refusal in &report.refused {
        eprintln!(
            "   refused    {} — {}",
            refusal.path.display(),
            refusal.reason
        );
    }
    warn_unignored(root, report.written.iter().chain(&report.unchanged));

    Ok(if report.refused.is_empty() { 0 } else { 1 })
}

/// `materialize --list`: the manifest, with each entry's current tree state.
fn materialize_list(root: &Path) -> Result<i32, String> {
    let store = MaterializeStore::open(root.join(".anneal"), root)
        .map_err(|e| format!("reading materialize manifest: {e}"))?;
    let entries = store.entries();
    if entries.is_empty() {
        println!("nothing materialized");
        return Ok(0);
    }
    for entry in &entries {
        let state = match store
            .tree_state(entry)
            .map_err(|e| format!("checking {}: {e}", entry.path.display()))?
        {
            TreeState::Intact => "intact",
            TreeState::Edited => "EDITED",
            TreeState::Missing => "MISSING",
        };
        println!("  {state:>7}  {}  ({})", entry.path.display(), entry.label);
    }
    Ok(0)
}

/// `materialize --clean [<target>]`: remove materialized files — all of them,
/// or only the named target's. Digest-guarded: an edited file is reported and
/// left in place unless `--force`.
fn materialize_clean(target: Option<&str>, force: bool, root: &Path) -> Result<i32, String> {
    let label = target
        .map(|t| Label::parse(t).map_err(|e| format!("invalid target {t:?}: {e}")))
        .transpose()?
        .map(|l| l.to_string());
    let _lock = lock::WorkspaceLock::acquire(&root.join(".anneal"))
        .map_err(|e| format!("acquiring workspace lock: {e}"))?;
    let mut store = MaterializeStore::open(root.join(".anneal"), root)
        .map_err(|e| format!("reading materialize manifest: {e}"))?;
    let report = store
        .clean(label.as_deref(), force)
        .map_err(|e| format!("cleaning materialized files: {e}"))?;
    if report.removed.is_empty() && report.refused.is_empty() {
        println!("nothing materialized");
        return Ok(0);
    }
    for path in &report.removed {
        println!("   removed  {}", path.display());
    }
    for refusal in &report.refused {
        eprintln!(
            "   kept     {} — {}",
            refusal.path.display(),
            refusal.reason
        );
    }
    Ok(if report.refused.is_empty() { 0 } else { 1 })
}

/// Resolve the target's provided files to `(workspace-relative destination,
/// content digest)`. Source-backed artifacts are already tree files — skipped.
fn provided_files(run: &PipelineRun, label: &Label) -> Result<Vec<(PathBuf, Digest)>, String> {
    let providers = run
        .graph
        .providers(label)
        .ok_or_else(|| format!("{label} was not analyzed"))?;
    let Some(file_set) = &providers.files else {
        return Ok(Vec::new());
    };
    let result_by_action: HashMap<&str, &ActionResult> = run
        .actions
        .iter()
        .map(|a| a.name())
        .zip(&run.results)
        .collect();
    let mut files = Vec::new();
    for artifact in &file_set.files {
        match &artifact.source {
            ArtifactSource::Source(_) => continue,
            ArtifactSource::Output { action, name } => {
                let result = result_by_action.get(action.as_str()).ok_or_else(|| {
                    format!("provider references an action not in the graph: {action:?}")
                })?;
                let digest = result
                    .outputs
                    .get(name)
                    .ok_or_else(|| format!("action {action:?} did not produce output {name:?}"))?;
                // The provider path is package-relative (the path consumers
                // materialize the file at), so the tree copy lands in the
                // producing target's package directory.
                let mut dest = PathBuf::new();
                if !label.package().is_empty() {
                    dest.push(label.package());
                }
                dest.push(&artifact.path);
                files.push((dest, *digest));
            }
        }
    }
    Ok(files)
}

/// The subset of `paths` tracked by git. Empty when not a git repo (nothing
/// is tracked) — materialize then proceeds without the tracked-file guard.
fn git_tracked<'p>(root: &Path, paths: impl Iterator<Item = &'p Path>) -> Vec<PathBuf> {
    let mut cmd = ProcessCommand::new("git");
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

/// Warn when materialized files are not gitignored: an unignored generated
/// copy shows up in `git status --porcelain`, dirtying the focus cone's edit
/// horizon on every build.
fn warn_unignored<'p>(root: &Path, paths: impl Iterator<Item = &'p PathBuf>) {
    let unignored: Vec<PathBuf> = paths
        .filter(|path| {
            // check-ignore exits 0 = ignored, 1 = not ignored, 128 = not a
            // repo / error (then there is no `git status` to pollute).
            ProcessCommand::new("git")
                .args(["check-ignore", "-q", "--"])
                .arg(path)
                .current_dir(root)
                .status()
                .ok()
                .and_then(|s| s.code())
                == Some(1)
        })
        .cloned()
        .collect();
    if !unignored.is_empty() {
        eprintln!(
            "note: not gitignored: {} — add to .gitignore so generated copies don't dirty \
             `git status` (and the focus cone)",
            join_paths(&unignored)
        );
    }
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Print one line per action: its cache/run status and name.
fn report_actions(actions: &[Action], results: &[ActionResult]) {
    for (action, result) in actions.iter().zip(results) {
        let status = if result.cache_hit {
            "CACHED"
        } else if result.success() {
            "ok"
        } else {
            "FAIL"
        };
        println!("  {status:>6}  {}", action.name());
    }
}

/// If `result` captured a `results.txt`, read the `ANNEAL_TEST_EXIT` marker and return
/// whether the test passed. `None` for non-test actions.
fn test_outcome(exec: &LocalExecutor, result: &ActionResult) -> Option<bool> {
    let digest = result.outputs.get("results.txt")?;
    let bytes = exec.cas().get(digest).ok().flatten()?;
    let text = String::from_utf8_lossy(&bytes);
    for line in text.lines() {
        if let Some(code) = line.strip_prefix("ANNEAL_TEST_EXIT=") {
            return Some(code.trim() == "0");
        }
    }
    None
}

/// Build a [`Configuration`] from the axis selectors, defaulting to the host config.
fn build_config(args: &ConfigArgs) -> Result<Configuration, String> {
    let mut axes = AxisValues::default();
    if let Some(s) = &args.opt_level {
        axes.opt_level = parse_opt_level(s)?;
    }
    if let Some(s) = &args.lto {
        axes.lto = parse_lto(s)?;
    }
    if let Some(s) = &args.debug_info {
        axes.debug_info = parse_debug_info(s)?;
    }
    if let Some(s) = &args.sanitizer {
        axes.sanitizer = parse_sanitizer(s)?;
    }
    if let Some(s) = &args.coverage {
        axes.coverage = parse_coverage(s)?;
    }
    if let Some(s) = &args.exec_mode {
        axes.exec_mode = parse_exec_mode(s)?;
    }
    // A host placeholder triple matches the analysis/test defaults; `--platform`
    // overrides it (cross-compilation wiring into the inner tools is a later step).
    let platform = match &args.platform {
        Some(triple) => Platform::new(triple.clone(), triple.clone()),
        None => Platform::new("host", "host"),
    };
    Ok(Configuration::new(platform, axes))
}

/// Normalize a flag value to the canonical `as_str` form (hyphens → underscores).
fn norm(s: &str) -> String {
    s.trim().replace('-', "_")
}

fn parse_exec_mode(s: &str) -> Result<ExecMode, String> {
    match norm(s).as_str() {
        "incremental" => Ok(ExecMode::Incremental),
        "hermetic" => Ok(ExecMode::Hermetic),
        other => Err(format!("invalid --exec-mode {other:?}")),
    }
}

fn parse_opt_level(s: &str) -> Result<OptLevel, String> {
    match norm(s).as_str() {
        "debug" => Ok(OptLevel::Debug),
        "release" => Ok(OptLevel::Release),
        "release_with_debuginfo" => Ok(OptLevel::ReleaseWithDebugInfo),
        other => Err(format!("invalid --opt-level {other:?}")),
    }
}

fn parse_lto(s: &str) -> Result<Lto, String> {
    match norm(s).as_str() {
        "off" => Ok(Lto::Off),
        "thin" => Ok(Lto::Thin),
        "full" => Ok(Lto::Full),
        other => Err(format!("invalid --lto {other:?}")),
    }
}

fn parse_debug_info(s: &str) -> Result<DebugInfo, String> {
    match norm(s).as_str() {
        "none" => Ok(DebugInfo::None),
        "line_tables_only" => Ok(DebugInfo::LineTablesOnly),
        "full" => Ok(DebugInfo::Full),
        other => Err(format!("invalid --debug-info {other:?}")),
    }
}

fn parse_sanitizer(s: &str) -> Result<Sanitizer, String> {
    match norm(s).as_str() {
        "none" => Ok(Sanitizer::None),
        "address" => Ok(Sanitizer::Address),
        "thread" => Ok(Sanitizer::Thread),
        "memory" => Ok(Sanitizer::Memory),
        "undefined" => Ok(Sanitizer::Undefined),
        other => Err(format!("invalid --sanitizer {other:?}")),
    }
}

fn parse_coverage(s: &str) -> Result<Coverage, String> {
    match norm(s).as_str() {
        "on" => Ok(Coverage::On),
        "off" => Ok(Coverage::Off),
        other => Err(format!("invalid --coverage {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_porcelain;
    use std::path::PathBuf;

    #[test]
    fn porcelain_parsing_covers_modified_untracked_and_renames() {
        let out =
            " M crates/a/src/lib.rs\n?? newfile.txt\nR  old/name.rs -> new/name.rs\nA  staged.rs\n";
        assert_eq!(
            parse_porcelain(out),
            vec![
                PathBuf::from("crates/a/src/lib.rs"),
                PathBuf::from("newfile.txt"),
                PathBuf::from("old/name.rs"),
                PathBuf::from("new/name.rs"),
                PathBuf::from("staged.rs"),
            ]
        );
    }
}
