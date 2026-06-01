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
use std::path::PathBuf;
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anneal_cas::Cas;
use anneal_core::Digest;
use anneal_snapshot::SnapshotStore;

use crate::action::{Action, CachePolicy, ExecutionMode, InputSource};
use crate::cache::{action_digest, ActionCache, StoredResult};
use crate::materializer;
use crate::sandbox::{self, SandboxSpec};

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
}

impl LocalExecutor {
    /// Open a local executor rooted at `store_root` (e.g. `.mybuild/`). The CAS,
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

    /// Execute an action graph: `actions` in dependency order (producers before
    /// consumers, as `anneal-analysis` emits). Each action's [`InputSource::Output`]
    /// references are resolved against the outputs produced earlier in the run, then
    /// the resolved action is executed (and cached) like any other. Returns the
    /// per-action results, aligned with `actions`.
    pub fn execute_graph(&self, actions: &[Action]) -> Result<Vec<ActionResult>, ExecError> {
        // (producing action name, output name) -> content digest
        let mut produced: HashMap<(String, String), Digest> = HashMap::new();
        let mut results = Vec::with_capacity(actions.len());

        for action in actions {
            let resolved = resolve_action(action, &produced)?;
            let result = self.execute(&resolved)?;
            for (output_name, digest) in &result.outputs {
                produced.insert((action.name().to_owned(), output_name.clone()), *digest);
            }
            results.push(result);
        }
        Ok(results)
    }
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
        if root.exists() {
            let _ = fs::remove_dir_all(&root);
        }
        let prepared = materializer::prepare_at(&self.cas, action, root)?;

        if restore {
            if let Some(key) = &action.snapshot_key {
                for path in &action.snapshot_paths {
                    self.snapshots
                        .restore(&self.cas, key, &prepared.cwd.join(path))?;
                }
            }
        }

        let spec = SandboxSpec {
            mode: action.execution_mode,
            cwd: &prepared.cwd,
            home: &prepared.home,
            tmp: &prepared.tmp,
            env: &action.env,
        };
        let mut child = sandbox::build_command(action, &spec)
            .spawn()
            .map_err(ExecError::Spawn)?;
        let status = wait_with_timeout(&mut child, action.timeout_ms)?;
        let exit_code = status.code().unwrap_or(-1);

        let outputs = if exit_code == 0 {
            let captured = materializer::capture(&self.cas, action, &prepared)?;
            if save {
                if let Some(key) = &action.snapshot_key {
                    for path in &action.snapshot_paths {
                        self.snapshots
                            .save(&self.cas, key, &prepared.cwd.join(path))?;
                    }
                }
            }
            captured
        } else {
            // Failed actions: no captured outputs, no snapshot saved.
            BTreeMap::new()
        };

        if !self.retain_sandboxes {
            let _ = fs::remove_dir_all(&prepared.root);
        }

        Ok(ActionResult {
            exit_code,
            outputs,
            cache_hit: false,
        })
    }

    /// The sandbox root for an action. Snapshot-based actions get a **stable** path
    /// (keyed by the snapshot key) so cold and warm runs use an identical working
    /// directory — outputs are then reproducible and directly comparable. Normal
    /// actions get a unique path so independent runs never collide.
    fn sandbox_root(&self, action: &Action, key: &Digest, snapshot_based: bool) -> PathBuf {
        if snapshot_based {
            let tag = action.snapshot_key.unwrap_or(*key);
            self.sandboxes.join(format!("snap-{}", &tag.to_hex()[..16]))
        } else {
            let nonce = SANDBOX_COUNTER.fetch_add(1, Ordering::Relaxed);
            self.sandboxes
                .join(format!("{}-{}", &key.to_hex()[..16], nonce))
        }
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
        // SnapshotBased owns the snapshot (restore + save); SnapshotAccelerated only
        // consumes one (restore, no save).
        let restore = matches!(
            action.cache_policy,
            CachePolicy::SnapshotBased | CachePolicy::SnapshotAccelerated
        );
        let save = matches!(action.cache_policy, CachePolicy::SnapshotBased);
        // Deterministic and snapshot-based actions are cacheable when sealed; permeable,
        // native, and snapshot-*accelerated* are not (§7.2, §8). SnapshotAccelerated is
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

        // Cache miss (or non-cacheable): run, restoring/saving the snapshot per policy.
        // Snapshot-involved actions get a stable sandbox path (keyed by the snapshot
        // key) so they share an owner's snapshot directory.
        let root = self.sandbox_root(action, &key, restore);
        let result = self.run_core(action, root, restore, save)?;

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
