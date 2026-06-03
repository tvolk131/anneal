//! The [`Executor`] interface and the local implementation.
//!
//! [`Executor::execute`] is the kernel's entire public surface. The orchestration
//! is: **check the cache → (miss) materialize inputs → run in the sandbox → capture
//! outputs → record the cache entry.** A caller never touches the materializer,
//! sandbox, or cache directly.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::io;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anneal_cas::Cas;
use anneal_core::Digest;
use anneal_snapshot::SnapshotStore;

use crate::action::{Action, CachePolicy, ExecutionMode, InputSource};
use crate::cache::{action_digest, ActionCache, StoredResult};
use crate::materializer;
use crate::sandbox::{self, SandboxSpec};
use crate::warm::{self, InputManifest};

/// Disambiguates per-run sandbox directories for normal (non-snapshot) actions.
static SANDBOX_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The outcome of executing an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionResult {
    /// Process exit code (`-1` if terminated without one).
    pub exit_code: i32,
    /// Declared outputs, by logical name, content-addressed in the CAS.
    pub outputs: BTreeMap<String, Digest>,
    /// Whether this result was served from the action cache (no re-execution).
    pub cache_hit: bool,
}

impl ActionResult {
    /// Whether the action succeeded (exit code 0).
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// Per-phase wall-clock for one *executed* action (a cache hit runs no phases and is
/// not recorded). Collected only when [`LocalExecutor::record_timings`] is set — a
/// diagnostic for profiling where a build spends its time (e.g. confirming the
/// `target/` snapshot save dominates a cold build's wrapping overhead, §20).
#[derive(Debug, Clone)]
pub struct PhaseTimings {
    pub action: String,
    /// Stage the declared inputs into a fresh sandbox (hardlink/clone from the CAS).
    pub materialize: Duration,
    /// Restore the snapshot into the sandbox (a cold start with no snapshot is ~0).
    pub restore: Duration,
    /// Spawn the command and wait for it — the inner tool's own time.
    pub run: Duration,
    /// Read declared outputs back into the CAS.
    pub capture: Duration,
    /// Save the snapshot (e.g. `target/`) into the CAS — only the snapshot owner.
    pub save: Duration,
    /// Remove the sandbox directory (scales with what the build wrote into it).
    pub teardown: Duration,
    /// Whole-action wall-clock (the sum of all phases).
    pub total: Duration,
}

/// Runs actions. Local and (future) remote executors share this interface so callers
/// never branch on where work runs (§7.1).
pub trait Executor {
    fn execute(&self, action: &Action) -> Result<ActionResult, ExecError>;
}

/// Executes actions on the local machine.
pub struct LocalExecutor {
    cas: Cas,
    cache: ActionCache,
    snapshots: SnapshotStore,
    sandboxes: PathBuf,
    retain_sandboxes: bool,
    /// Max actions to run concurrently in [`execute_graph`]. Defaults to the machine's
    /// available parallelism.
    parallelism: usize,
    /// When set, every executed action appends its [`PhaseTimings`] here (diagnostic).
    timings: Option<Mutex<Vec<PhaseTimings>>>,
    /// Opt-in warm-sandbox reuse for snapshot owners (`docs/sandboxing.md` §5). Off by
    /// default — it changes the isolation model from fresh-per-build to in-place reuse.
    warm_reuse: bool,
    /// Persistent warm working trees, keyed by snapshot key: `warm/<key16>/`.
    warm: PathBuf,
    /// Warm-dir bookkeeping (manifest = commit record), kept out of the working tree.
    warm_meta: PathBuf,
    /// Per-snapshot-key locks: same-key owners share one warm dir and serialize on it
    /// (§5.3.1), while different keys run concurrently.
    warm_locks: Mutex<HashMap<Digest, Arc<Mutex<()>>>>,
}

impl LocalExecutor {
    /// Open a local executor rooted at `store_root` (e.g. `.anneal/`). The CAS,
    /// action cache, snapshot store, and sandbox roots are created underneath and
    /// share one volume so hardlink materialization works (§3.4).
    pub fn new(store_root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = store_root.into();
        let cas = Cas::open(root.join("cas"))?;
        let cache = ActionCache::open(root.join("cache"))?;
        let snapshots = SnapshotStore::open(root.join("snapshots"))?;
        let sandboxes = root.join("sandboxes");
        fs::create_dir_all(&sandboxes)?;
        Ok(LocalExecutor {
            cas,
            cache,
            snapshots,
            sandboxes,
            retain_sandboxes: false,
            parallelism: default_parallelism(),
            timings: None,
            warm_reuse: false,
            warm: root.join("warm"),
            warm_meta: root.join("warm-meta"),
            warm_locks: Mutex::new(HashMap::new()),
        })
    }

    /// The CAS, so callers can stage inputs and read outputs by digest.
    pub fn cas(&self) -> &Cas {
        &self.cas
    }

    /// Keep sandbox directories after execution (for debugging). Off by default.
    pub fn retain_sandboxes(mut self, retain: bool) -> Self {
        self.retain_sandboxes = retain;
        self
    }

    /// Cap the number of actions [`execute_graph`] runs concurrently. Clamped to at
    /// least 1. Defaults to the machine's available parallelism.
    pub fn jobs(mut self, jobs: usize) -> Self {
        self.parallelism = jobs.max(1);
        self
    }

    /// Record per-phase [`PhaseTimings`] for every executed action (off by default).
    /// A diagnostic for profiling; drain with [`LocalExecutor::take_timings`].
    pub fn record_timings(mut self) -> Self {
        self.timings = Some(Mutex::new(Vec::new()));
        self
    }

    /// Enable warm-sandbox reuse for snapshot owners (`docs/sandboxing.md` §5; off by
    /// default). On a cache miss, a `SnapshotBased` action reuses a persistent per-key
    /// working tree — syncing only changed inputs and keeping `target/` in place —
    /// instead of a fresh sandbox + snapshot round-trip.
    pub fn warm_reuse(mut self) -> Self {
        self.warm_reuse = true;
        self
    }

    /// Drain the recorded phase timings (empty unless [`LocalExecutor::record_timings`]
    /// was set). Cache hits are not recorded — only actions that actually executed.
    pub fn take_timings(&self) -> Vec<PhaseTimings> {
        match &self.timings {
            Some(m) => std::mem::take(&mut m.lock().unwrap()),
            None => Vec::new(),
        }
    }

    /// Execute an action graph, running independent actions **concurrently** (up to
    /// [`LocalExecutor::jobs`]). The input order need not be topological: the
    /// dependency DAG is derived from each action's edges (see [`build_edges`]), and an
    /// action runs only once all its dependencies have completed. Each action's
    /// [`InputSource::Output`] references are resolved against the outputs produced by
    /// the run before it executes (and is cached) like any other. Returns the
    /// per-action results, **aligned with `actions`** regardless of scheduling order.
    ///
    /// On the first execution *error* (a dangling reference, spawn/IO failure — not a
    /// merely non-zero exit, which is a normal result) the scheduler stops dispatching
    /// new work, lets in-flight actions drain, and returns that error.
    pub fn execute_graph(&self, actions: &[Action]) -> Result<Vec<ActionResult>, ExecError> {
        if actions.is_empty() {
            return Ok(Vec::new());
        }
        let edges = build_edges(actions)?;
        let workers = self.parallelism.min(actions.len()).max(1);

        let state = Mutex::new(SchedState::new(actions.len(), &edges));
        let progress = Condvar::new();

        thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    while let Some((idx, resolved)) = next_task(&state, &progress, actions) {
                        let outcome = self.execute(&resolved);
                        complete(&state, &progress, &edges, idx, actions[idx].name(), outcome);
                    }
                });
            }
        });

        let mut state = state.into_inner().unwrap();
        if let Some(err) = state.failed.take() {
            return Err(err);
        }
        // Every action completed successfully → every slot is filled.
        Ok(state.results.into_iter().map(Option::unwrap).collect())
    }
}

/// The dependency DAG of an action graph, as forward and reverse adjacency by index.
struct Edges {
    /// `deps[i]` = indices `i` depends on (its dependencies must finish before it runs).
    deps: Vec<Vec<usize>>,
    /// `dependents[i]` = indices that depend on `i` (unblocked when `i` finishes).
    dependents: Vec<Vec<usize>>,
}

/// Derive the dependency edges of an action graph from two sources (§ parallel-execution
/// design): **data edges** — an [`InputSource::Output`] makes the consumer depend on the
/// producer — and **snapshot-owner edges** — a [`CachePolicy::SnapshotConsuming`] action
/// depends on the [`CachePolicy::SnapshotBased`] action that *owns* (saves) the snapshot
/// for its `snapshot_key`, since it cannot restore a snapshot that has not been saved.
/// This is the one place snapshot ordering is reconstructed; promoting it to a declared
/// edge in the action model is deferred (see TODO).
fn build_edges(actions: &[Action]) -> Result<Edges, ExecError> {
    // action name -> index, for resolving data edges.
    let mut by_name: HashMap<&str, usize> = HashMap::with_capacity(actions.len());
    for (i, a) in actions.iter().enumerate() {
        by_name.insert(a.name(), i);
    }
    // snapshot key -> the index of its owning SnapshotBased action.
    let mut owner_of: HashMap<Digest, usize> = HashMap::new();
    for (i, a) in actions.iter().enumerate() {
        if a.cache_policy == CachePolicy::SnapshotBased {
            if let Some(key) = a.snapshot_key {
                owner_of.insert(key, i);
            }
        }
    }

    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); actions.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); actions.len()];
    let add_edge = |from: usize, to: usize, deps: &mut Vec<Vec<usize>>, dependents: &mut Vec<Vec<usize>>| {
        // `from` depends on `to`; dedup so a multi-output producer is listed once.
        if from != to && !deps[from].contains(&to) {
            deps[from].push(to);
            dependents[to].push(from);
        }
    };

    for (i, a) in actions.iter().enumerate() {
        // Data edges.
        for input in a.inputs.values() {
            if let InputSource::Output { action: producer, name } = &input.source {
                let &p = by_name.get(producer.as_str()).ok_or_else(|| ExecError::UnresolvedInput {
                    action: producer.clone(),
                    output: name.clone(),
                })?;
                add_edge(i, p, &mut deps, &mut dependents);
            }
        }
        // Snapshot-owner edge.
        if a.cache_policy == CachePolicy::SnapshotConsuming {
            if let Some(key) = a.snapshot_key {
                if let Some(&owner) = owner_of.get(&key) {
                    add_edge(i, owner, &mut deps, &mut dependents);
                }
                // No owner in this graph → a cold-start restore, which the snapshot
                // store handles gracefully; nothing to order against.
            }
        }
    }

    Ok(Edges { deps, dependents })
}

/// Mutable scheduler state, guarded by one mutex; workers coordinate via a condvar.
struct SchedState {
    /// Remaining unfinished dependencies per action; an action is ready at 0.
    pending: Vec<usize>,
    /// Indices ready to dispatch (all dependencies finished).
    ready: Vec<usize>,
    /// Producer output → content digest, accumulated as actions complete.
    produced: HashMap<(String, String), Digest>,
    /// Per-index results, filled on completion; `None` until then.
    results: Vec<Option<ActionResult>>,
    /// Actions still to complete (success path). Reaches 0 iff the whole graph ran.
    remaining: usize,
    /// Actions currently executing.
    running: usize,
    /// First execution error; once set, no new work is dispatched.
    failed: Option<ExecError>,
}

impl SchedState {
    fn new(n: usize, edges: &Edges) -> Self {
        let pending: Vec<usize> = edges.deps.iter().map(Vec::len).collect();
        // Seed the ready set with every action that has no dependencies.
        let ready: Vec<usize> = (0..n).filter(|&i| pending[i] == 0).collect();
        SchedState {
            pending,
            ready,
            produced: HashMap::new(),
            results: (0..n).map(|_| None).collect(),
            remaining: n,
            running: 0,
            failed: None,
        }
    }
}

/// Claim the next ready action under the lock, resolving its [`InputSource::Output`]
/// inputs against outputs produced so far. Blocks until work is available, and returns
/// `None` when the worker should exit: the graph is done, an error was recorded, or no
/// progress is possible (a cycle — defensive; analysis emits a DAG).
fn next_task(
    state: &Mutex<SchedState>,
    progress: &Condvar,
    actions: &[Action],
) -> Option<(usize, Action)> {
    let mut st = state.lock().unwrap();
    loop {
        if st.failed.is_some() || st.remaining == 0 {
            return None;
        }
        if let Some(idx) = st.ready.pop() {
            match resolve_action(&actions[idx], &st.produced) {
                Ok(resolved) => {
                    st.running += 1;
                    return Some((idx, resolved));
                }
                Err(e) => {
                    // A dependency finished without producing the referenced output
                    // (e.g. it exited non-zero). Treat as a graph error and stop.
                    st.failed.get_or_insert(e);
                    progress.notify_all();
                    return None;
                }
            }
        }
        if st.running == 0 {
            // Nothing ready and nothing running, yet work remains: the only way that
            // happens is an unsatisfiable dependency cycle.
            st.failed.get_or_insert(ExecError::DependencyCycle);
            progress.notify_all();
            return None;
        }
        st = progress.wait(st).unwrap();
    }
}

/// Record an action's outcome under the lock and unblock its dependents. A non-zero
/// exit is a normal result (its empty output set surfaces later as an `UnresolvedInput`
/// for any consumer that needed it); only an `Err` aborts the run.
fn complete(
    state: &Mutex<SchedState>,
    progress: &Condvar,
    edges: &Edges,
    idx: usize,
    name: &str,
    outcome: Result<ActionResult, ExecError>,
) {
    let mut st = state.lock().unwrap();
    st.running -= 1;
    match outcome {
        Err(e) => {
            st.failed.get_or_insert(e);
        }
        Ok(result) => {
            for (output_name, digest) in &result.outputs {
                st.produced.insert((name.to_owned(), output_name.clone()), *digest);
            }
            if st.failed.is_none() {
                for &dep in &edges.dependents[idx] {
                    st.pending[dep] -= 1;
                    if st.pending[dep] == 0 {
                        st.ready.push(dep);
                    }
                }
            }
            st.results[idx] = Some(result);
            st.remaining -= 1;
        }
    }
    progress.notify_all();
}

/// The machine's available parallelism, or 1 if it cannot be determined.
fn default_parallelism() -> usize {
    std::thread::available_parallelism().map(NonZeroUsize::get).unwrap_or(1)
}

/// Return a copy of `action` with every [`InputSource::Output`] replaced by the
/// concrete blob produced earlier in the run.
fn resolve_action(
    action: &Action,
    produced: &HashMap<(String, String), Digest>,
) -> Result<Action, ExecError> {
    let mut resolved = action.clone();
    for input in resolved.inputs.values_mut() {
        if let InputSource::Output {
            action: producer,
            name,
        } = &input.source
        {
            let digest = produced
                .get(&(producer.clone(), name.clone()))
                .copied()
                .ok_or_else(|| ExecError::UnresolvedInput {
                    action: producer.clone(),
                    output: name.clone(),
                })?;
            input.source = InputSource::Blob(digest);
        }
    }
    Ok(resolved)
}

impl LocalExecutor {
    /// Materialize → (optionally restore snapshot) → run → capture → (optionally save
    /// snapshot). The shared core of cached execution and verification. The sandbox
    /// at `root` is recreated fresh each call.
    fn run_core(
        &self,
        action: &Action,
        root: PathBuf,
        restore: bool,
        save: bool,
    ) -> Result<ActionResult, ExecError> {
        let started = Instant::now();
        if root.exists() {
            let _ = fs::remove_dir_all(&root);
        }
        let prepared = materializer::prepare_at(&self.cas, action, root)?;
        let t_materialize = started.elapsed();

        let restore_start = Instant::now();
        if restore {
            if let Some(key) = &action.snapshot_key {
                for path in &action.snapshot_paths {
                    self.snapshots
                        .restore(&self.cas, key, &prepared.cwd.join(path))?;
                }
            }
        }
        let t_restore = restore_start.elapsed();

        let spec = SandboxSpec {
            mode: action.execution_mode,
            cwd: &prepared.cwd,
            home: &prepared.home,
            tmp: &prepared.tmp,
            env: &action.env,
        };
        let run_start = Instant::now();
        let mut child = sandbox::build_command(action, &spec)
            .spawn()
            .map_err(ExecError::Spawn)?;
        let status = wait_with_timeout(&mut child, action.timeout_ms)?;
        let exit_code = status.code().unwrap_or(-1);
        let t_run = run_start.elapsed();

        let mut t_capture = Duration::ZERO;
        let mut t_save = Duration::ZERO;
        let outputs = if exit_code == 0 {
            let capture_start = Instant::now();
            let captured = materializer::capture(&self.cas, action, &prepared)?;
            t_capture = capture_start.elapsed();
            if save {
                let save_start = Instant::now();
                if let Some(key) = &action.snapshot_key {
                    for path in &action.snapshot_paths {
                        self.snapshots
                            .save(&self.cas, key, &prepared.cwd.join(path))?;
                    }
                }
                t_save = save_start.elapsed();
            }
            captured
        } else {
            // Failed actions: no captured outputs, no snapshot saved.
            BTreeMap::new()
        };

        let teardown_start = Instant::now();
        if !self.retain_sandboxes {
            let _ = fs::remove_dir_all(&prepared.root);
        }
        let t_teardown = teardown_start.elapsed();

        if let Some(sink) = &self.timings {
            sink.lock().unwrap().push(PhaseTimings {
                action: action.name().to_owned(),
                materialize: t_materialize,
                restore: t_restore,
                run: t_run,
                capture: t_capture,
                save: t_save,
                teardown: t_teardown,
                total: started.elapsed(),
            });
        }

        Ok(ActionResult {
            exit_code,
            outputs,
            cache_hit: false,
        })
    }

    /// The sandbox root for an action: a **unique** path per run, including for
    /// snapshot actions. A snapshot is restored from the immutable store into this
    /// private directory, so snapshot-*consuming* actions sharing one key never share a
    /// working directory and can run concurrently (the owner-ordering edge in
    /// [`build_edges`] guarantees the snapshot exists first). The cold-vs-warm
    /// reproducibility comparison uses a stable, caller-named path via
    /// [`LocalExecutor::run_uncached`] instead, so it is unaffected.
    ///
    /// Note: `nonce` is process-local, so two concurrent `anneal` processes can still
    /// collide on this path — closed later by the workspace lock (see TODO).
    fn sandbox_root(&self, key: &Digest) -> PathBuf {
        let nonce = SANDBOX_COUNTER.fetch_add(1, Ordering::Relaxed);
        self.sandboxes
            .join(format!("{}-{}", &key.to_hex()[..16], nonce))
    }

    /// The per-snapshot-key serialization lock for a warm dir, created on first use.
    fn warm_key_lock(&self, key: &Digest) -> Arc<Mutex<()>> {
        self.warm_locks
            .lock()
            .unwrap()
            .entry(*key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Run a snapshot-owner action against a **persistent warm working tree** (§5).
    /// Reuse it in place when a committed manifest exists (sync only changed inputs, keep
    /// `target/` warm, no restore, no teardown); otherwise cold-populate it (wipe →
    /// materialize all inputs → restore the last good snapshot). The snapshot is still
    /// saved to the CAS so consumers and other machines can restore it. Same-key owners
    /// serialize on the dir via [`LocalExecutor::warm_key_lock`].
    fn run_warm(&self, action: &Action) -> Result<ActionResult, ExecError> {
        let started = Instant::now();
        let skey = action
            .snapshot_key
            .expect("run_warm is only called for snapshot owners, which have a snapshot_key");
        let tag = skey.to_hex();
        let tag = &tag[..16];
        let warm_dir = self.warm.join(tag);
        let manifest_path = self.warm_meta.join(tag).join("inputs");

        // Same-key owners share this warm dir; serialize on it (different keys run free).
        let key_lock = self.warm_key_lock(&skey);
        let _guard = key_lock.lock().unwrap();

        // The desired declared inputs as path -> digest (the action is already resolved).
        let desired: BTreeMap<PathBuf, Digest> = action
            .inputs
            .values()
            .filter_map(|i| match &i.source {
                InputSource::Blob(d) => Some((i.path.clone(), *d)),
                InputSource::Output { .. } => None,
            })
            .collect();

        // Reuse iff a committed manifest exists AND the working tree is present.
        let baseline = match InputManifest::load(&manifest_path)? {
            Some(old) if warm_dir.exists() => Some(old),
            _ => None,
        };
        // BEGIN the transaction: the manifest doubles as the commit record (§5.4), so
        // clear it before mutating — a crash then leaves no baseline and the next run
        // cold-populates rather than trusting a half-synced tree.
        let _ = fs::remove_file(&manifest_path);

        let t_materialize;
        let mut t_restore = Duration::ZERO;
        let prepared = if let Some(old) = baseline {
            // Reuse: keep target/ and unchanged sources in place; refresh only scratch.
            let m = Instant::now();
            let _ = fs::remove_dir_all(warm_dir.join(".home"));
            let _ = fs::remove_dir_all(warm_dir.join(".tmp"));
            let prepared = materializer::layout(action, warm_dir.clone())?;
            warm::sync(&self.cas, &prepared.cwd, &old, &desired)?;
            t_materialize = m.elapsed();
            prepared
        } else {
            // Cold-populate: wipe, materialize all inputs, restore the last good snapshot.
            let m = Instant::now();
            if warm_dir.exists() {
                let _ = fs::remove_dir_all(&warm_dir);
            }
            let prepared = materializer::prepare_at(&self.cas, action, warm_dir.clone())?;
            t_materialize = m.elapsed();
            let r = Instant::now();
            for path in &action.snapshot_paths {
                self.snapshots.restore(&self.cas, &skey, &prepared.cwd.join(path))?;
            }
            t_restore = r.elapsed();
            prepared
        };

        let spec = SandboxSpec {
            mode: action.execution_mode,
            cwd: &prepared.cwd,
            home: &prepared.home,
            tmp: &prepared.tmp,
            env: &action.env,
        };
        let run_start = Instant::now();
        let mut child = sandbox::build_command(action, &spec)
            .spawn()
            .map_err(ExecError::Spawn)?;
        let status = wait_with_timeout(&mut child, action.timeout_ms)?;
        let exit_code = status.code().unwrap_or(-1);
        let t_run = run_start.elapsed();

        let mut t_capture = Duration::ZERO;
        let mut t_save = Duration::ZERO;
        let outputs = if exit_code == 0 {
            let c = Instant::now();
            let captured = materializer::capture(&self.cas, action, &prepared)?;
            t_capture = c.elapsed();
            let s = Instant::now();
            for path in &action.snapshot_paths {
                self.snapshots.save(&self.cas, &skey, &prepared.cwd.join(path))?;
            }
            t_save = s.elapsed();
            // COMMIT: the atomically-written manifest's presence marks the tree clean.
            InputManifest::new(desired).save_atomic(&manifest_path)?;
            captured
        } else {
            // Failed build: leave the manifest absent → next run cold-populates. No teardown.
            BTreeMap::new()
        };

        if let Some(sink) = &self.timings {
            sink.lock().unwrap().push(PhaseTimings {
                action: action.name().to_owned(),
                materialize: t_materialize,
                restore: t_restore,
                run: t_run,
                capture: t_capture,
                save: t_save,
                teardown: Duration::ZERO,
                total: started.elapsed(),
            });
        }

        Ok(ActionResult { exit_code, outputs, cache_hit: false })
    }

    /// Run an action **outside the action cache**, in a caller-named sandbox, with
    /// explicit snapshot restore/save. This is the primitive the correctness-neutral
    /// verification harness uses to run an action cold vs. snapshot-warm and compare.
    pub fn run_uncached(
        &self,
        action: &Action,
        sandbox_name: &str,
        restore: bool,
        save: bool,
    ) -> Result<ActionResult, ExecError> {
        guard_resolved(action)?;
        let root = self.sandboxes.join(sandbox_name);
        self.run_core(action, root, restore, save)
    }
}

impl Executor for LocalExecutor {
    fn execute(&self, action: &Action) -> Result<ActionResult, ExecError> {
        guard_resolved(action)?;

        let key = action_digest(action);
        // Two orthogonal properties (§5 of docs/rules.md):
        //   restore  — bring a snapshot into the sandbox before running
        //   save     — write the snapshot back afterward (only the owner does)
        // SnapshotBased owns the snapshot (restore + save); SnapshotConsuming only
        // consumes one (restore, no save).
        let restore = matches!(
            action.cache_policy,
            CachePolicy::SnapshotBased | CachePolicy::SnapshotConsuming
        );
        let save = matches!(action.cache_policy, CachePolicy::SnapshotBased);
        // Deterministic and snapshot-based actions are cacheable when sealed; permeable,
        // native, and snapshot-*consuming* are not (§7.2, §8). SnapshotConsuming is
        // deliberately excluded: its output is not trusted reproducible, so it never
        // skips. A snapshot is a separate accelerator for the case where an action runs.
        let cacheable = matches!(
            action.cache_policy,
            CachePolicy::Deterministic | CachePolicy::SnapshotBased
        ) && matches!(action.execution_mode, ExecutionMode::Sealed);

        if cacheable {
            if let Some(stored) = self.cache.lookup(&key)? {
                return Ok(ActionResult {
                    exit_code: stored.exit_code,
                    outputs: stored.outputs,
                    cache_hit: true,
                });
            }
        }

        // Cache miss (or non-cacheable): run the action. A snapshot *owner* reuses its
        // persistent warm working tree when that's enabled (§5); everything else gets a
        // fresh unique sandbox with the snapshot restored from the store.
        let warm_eligible = self.warm_reuse
            && matches!(action.cache_policy, CachePolicy::SnapshotBased)
            && action.snapshot_key.is_some();
        let result = if warm_eligible {
            self.run_warm(action)?
        } else {
            let root = self.sandbox_root(&key);
            self.run_core(action, root, restore, save)?
        };

        if result.exit_code == 0 && cacheable {
            self.cache.insert(
                &key,
                &StoredResult {
                    exit_code: result.exit_code,
                    outputs: result.outputs.clone(),
                },
            )?;
        }
        Ok(result)
    }
}

/// A single action must be fully resolved (every input a concrete blob); Output
/// references are resolved by `execute_graph` first.
fn guard_resolved(action: &Action) -> Result<(), ExecError> {
    for input in action.inputs.values() {
        if let InputSource::Output { action: a, name } = &input.source {
            return Err(ExecError::UnresolvedInput {
                action: a.clone(),
                output: name.clone(),
            });
        }
    }
    Ok(())
}

/// Wait for `child`, killing it if it exceeds `timeout_ms`.
fn wait_with_timeout(child: &mut Child, timeout_ms: u64) -> Result<ExitStatus, ExecError> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Some(status) = child.try_wait().map_err(ExecError::Io)? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ExecError::Timeout { timeout_ms });
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Failure executing an action. (Distinct from a *failed action*, which is a
/// successful execution with a non-zero exit code.)
#[derive(Debug)]
pub enum ExecError {
    /// A filesystem/CAS error during materialization, capture, or caching.
    Io(io::Error),
    /// The command could not be spawned.
    Spawn(io::Error),
    /// A declared output was not produced.
    MissingOutput(String),
    /// An input referenced an action output that has not been produced (the producer
    /// did not run before this action, or the reference is dangling).
    UnresolvedInput { action: String, output: String },
    /// The action exceeded its timeout and was killed.
    Timeout { timeout_ms: u64 },
    /// The action graph contains a dependency cycle, so no execution order exists.
    DependencyCycle,
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::Io(e) => write!(f, "I/O error: {e}"),
            ExecError::Spawn(e) => write!(f, "failed to spawn command: {e}"),
            ExecError::MissingOutput(name) => {
                write!(f, "action did not produce declared output {name:?}")
            }
            ExecError::UnresolvedInput { action, output } => {
                write!(f, "input references unproduced output {output:?} of action {action:?}")
            }
            ExecError::Timeout { timeout_ms } => {
                write!(f, "action exceeded its {timeout_ms}ms timeout")
            }
            ExecError::DependencyCycle => {
                write!(f, "action graph contains a dependency cycle")
            }
        }
    }
}

impl From<io::Error> for ExecError {
    fn from(e: io::Error) -> Self {
        ExecError::Io(e)
    }
}

impl std::error::Error for ExecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ExecError::Io(e) | ExecError::Spawn(e) => Some(e),
            _ => None,
        }
    }
}
