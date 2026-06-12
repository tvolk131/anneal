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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anneal_analysis::Analyzer;
use anneal_core::{
    AxisValues, Configuration, Coverage, DebugInfo, ExecMode, Label, Lto, OptLevel, Platform,
    Sanitizer,
};
use anneal_exec::{Action, ActionResult, LocalExecutor};
use anneal_loader::{load_closure, load_workspace};
use anneal_rules::builtin_rules;
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
        ),
        Command::Test { target } => test(
            &target,
            &config,
            &root,
            cli.config.jobs,
            cli.config.require_enforced,
        ),
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
) -> Result<i32, String> {
    let (actions, results, exec) = analyze_and_run(target, config, root, jobs, require_enforced)?;
    report_actions(&actions, &results);

    let failed = results.iter().filter(|r| !r.success()).count();
    let cached = results.iter().filter(|r| r.cache_hit).count();
    let _ = exec; // keep the executor (and its stores) alive through reporting
    if failed > 0 {
        eprintln!("build FAILED — {failed}/{} action(s) failed", actions.len());
        Ok(1)
    } else {
        println!("build ok — {} action(s) ({cached} cached)", actions.len());
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
) -> Result<i32, String> {
    let (actions, results, exec) = analyze_and_run(target, config, root, jobs, require_enforced)?;
    report_actions(&actions, &results);

    // Test actions are rule-agnostic: any action that captured `results.txt` and wrote
    // the `ANNEAL_TEST_EXIT` marker (cargo's test-run, pnpm's test kind). Structured
    // per-case parsing is a later increment; here we report pass/fail per test action.
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut saw_test = false;
    for (action, result) in actions.iter().zip(&results) {
        if let Some(ok) = test_outcome(&exec, result) {
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
    let action_failures = results.iter().filter(|r| !r.success()).count();
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

/// The shared pipeline: parse the label, load its package, analyze, execute the graph.
fn analyze_and_run(
    target: &str,
    config: &Configuration,
    root: &Path,
    jobs: Option<usize>,
    require_enforced: bool,
) -> Result<(Vec<Action>, Vec<ActionResult>, LocalExecutor), String> {
    let label = Label::parse(target).map_err(|e| format!("invalid target {target:?}: {e}"))?;
    let registry = builtin_rules();
    // A mutating command takes the coarse exclusive workspace lock for its whole run, so
    // concurrent `anneal` processes can't collide on shared warm dirs / sandboxes. Held
    // until this function returns (after execute_graph); released on drop. Read-only
    // commands (`affected`/`why`) deliberately do not acquire it. (See lock.rs.)
    let _lock = lock::WorkspaceLock::acquire(&root.join(".anneal"))
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
    let analyzed = Analyzer::new(&graph, &registry, config, root, exec.cas())
        .with_executor(&exec)
        .analyze(&label)
        .map_err(|e| e.to_string())?;
    let actions: Vec<Action> = analyzed.actions().cloned().collect();
    let results = exec.execute_graph(&actions).map_err(|e| e.to_string())?;
    Ok((actions, results, exec))
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
