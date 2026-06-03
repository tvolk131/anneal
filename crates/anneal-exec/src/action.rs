//! The action specification (§19.1) and its construction.
//!
//! An [`Action`] is a pure description of work: a command, its declared inputs and
//! outputs, its environment, and the configuration it runs under. It is built only
//! through [`Action::builder`] so the defaults (sealed, deterministic, host config)
//! are applied consistently. Fields are `pub(crate)` — readable by the cache,
//! materializer, and sandbox modules, but constructible from outside only via the
//! builder.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anneal_core::{Axis, AxisValues, Configuration, Digest, Platform};

/// Where an input's content comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputSource {
    /// A concrete CAS blob, known at analysis time (a source file, a `filegroup`).
    Blob(Digest),
    /// Another action's named output, resolved to a blob at execution time. The
    /// referenced action is identified by its (graph-unique) [`Action::name`].
    Output { action: String, name: String },
}

/// A declared input: content (a [`InputSource`]) to be materialized at `path`
/// (relative to the action's working directory) inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub path: PathBuf,
    pub source: InputSource,
}

/// Isolation level for a running action (§7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionMode {
    /// Hermetic; strict input isolation. The default and the only cacheable mode.
    #[default]
    Sealed,
    /// Relaxed isolation for actions needing access beyond declared inputs.
    /// Not cacheable.
    Permeable,
    /// Direct execution with no isolation (used by `anneal exec`). Not cacheable.
    Native,
}

impl ExecutionMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ExecutionMode::Sealed => "sealed",
            ExecutionMode::Permeable => "permeable",
            ExecutionMode::Native => "native",
        }
    }
}

/// How an action's result may be cached (§8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CachePolicy {
    /// Pure function of declared inputs; result is cacheable.
    #[default]
    Deterministic,
    /// Never cached.
    NonCacheable,
    /// Cached with a stateful snapshot (§8.2): the action **owns** the snapshot — it
    /// restores it before running and *saves* it after — and its result is action-
    /// cacheable (it is verified reproducible). cargo's `target/` and pnpm's `install`.
    SnapshotBased,
    /// **Restores** a snapshot another action owns (read-only — never saves it) but is
    /// **not** action-cacheable: the action always re-runs because its output is not
    /// trusted reproducible (`docs/rules.md` §5). The honest default for an opaque
    /// script that needs `node_modules` present but whose result we won't reuse. The
    /// promotion to `SnapshotBased` is earned via verification, never asserted.
    SnapshotConsuming,
}

impl CachePolicy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CachePolicy::Deterministic => "deterministic",
            CachePolicy::NonCacheable => "non_cacheable",
            CachePolicy::SnapshotBased => "snapshot_based",
            CachePolicy::SnapshotConsuming => "snapshot_consuming",
        }
    }
}

/// A unit of executable work. See the module docs; build with [`Action::builder`].
#[derive(Debug, Clone)]
pub struct Action {
    pub(crate) name: String,
    /// argv; `command[0]` is the program.
    pub(crate) command: Vec<String>,
    /// Declared inputs keyed by logical name (sorted for deterministic keys).
    pub(crate) inputs: BTreeMap<String, Input>,
    /// Declared outputs: logical name → path relative to the working directory.
    pub(crate) outputs: BTreeMap<String, PathBuf>,
    /// Additional environment variables (names AND values enter the cache key, §7.4).
    pub(crate) env: BTreeMap<String, String>,
    /// Working directory relative to the sandbox root (default ".").
    pub(crate) working_directory: PathBuf,
    pub(crate) execution_mode: ExecutionMode,
    pub(crate) cache_policy: CachePolicy,
    /// The configuration this action runs under (§3.3).
    pub(crate) config: Configuration,
    /// Which axes this action's cache key depends on — drives trimming (§6.2).
    pub(crate) consumed_axes: Vec<Axis>,
    pub(crate) timeout_ms: u64,
    /// Mutable cache directories to snapshot (e.g. `["target"]`), relative to the
    /// working directory. Empty unless the action uses snapshot-based caching.
    pub(crate) snapshot_paths: Vec<PathBuf>,
    /// The coarse snapshot key (e.g. a hash of toolchain+lockfile+triple+profile).
    /// An accelerator only — deliberately **not** part of the action cache key.
    pub(crate) snapshot_key: Option<Digest>,
    /// Whether the snapshot is **shared** (consumed by other actions → saved to the CAS
    /// every build) or **private** (internal incremental state, never consumed → not
    /// saved per build; warm reuse keeps it in place). Only meaningful for
    /// [`CachePolicy::SnapshotBased`]. Default `true` (conservative). See §5.8.1.
    pub(crate) snapshot_shared: bool,
    /// Whether this action's output depends on the target platform. `true` for most
    /// actions (the platform is part of identity); `false` for platform-independent
    /// ones like `nickel_eval`, whose result is shared across all platforms (§6.3).
    pub(crate) platform_sensitive: bool,
}

impl Action {
    /// Start building an action. `command` must be non-empty (argv).
    pub fn builder(
        name: impl Into<String>,
        command: impl IntoIterator<Item = impl Into<String>>,
    ) -> ActionBuilder {
        ActionBuilder {
            action: Action {
                name: name.into(),
                command: command.into_iter().map(Into::into).collect(),
                inputs: BTreeMap::new(),
                outputs: BTreeMap::new(),
                env: BTreeMap::new(),
                working_directory: PathBuf::from("."),
                execution_mode: ExecutionMode::default(),
                cache_policy: CachePolicy::default(),
                config: default_host_config(),
                consumed_axes: Vec::new(),
                timeout_ms: 600_000,
                snapshot_paths: Vec::new(),
                snapshot_key: None,
                snapshot_shared: true,
                platform_sensitive: true,
            },
        }
    }

    /// The action's name. Excluded from the cache key (§8.1), but it is the
    /// **graph-unique identity** other actions use to reference this action's
    /// outputs ([`InputSource::Output`]), so it must be unique within a graph.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// A placeholder host configuration used until analysis supplies a real one. The
/// triple is intentionally a stable string so default-config actions cache stably.
fn default_host_config() -> Configuration {
    Configuration::new(Platform::new("host", "host"), AxisValues::default())
}

/// Fluent builder for [`Action`]. Each setter consumes and returns `self`.
#[derive(Debug, Clone)]
pub struct ActionBuilder {
    action: Action,
}

impl ActionBuilder {
    /// Declare an input from a concrete CAS blob: materialize `digest` at `path`
    /// (relative to the working directory) under the logical `name`.
    pub fn input(mut self, name: impl Into<String>, path: impl Into<PathBuf>, digest: Digest) -> Self {
        self.action.inputs.insert(
            name.into(),
            Input {
                path: path.into(),
                source: InputSource::Blob(digest),
            },
        );
        self
    }

    /// Declare an input from another action's output: at execution time the producer
    /// `action_id`'s output `output_name` is resolved to a blob and materialized at
    /// `path`. This is the inter-action edge of the action graph.
    pub fn input_from_output(
        mut self,
        name: impl Into<String>,
        path: impl Into<PathBuf>,
        action_id: impl Into<String>,
        output_name: impl Into<String>,
    ) -> Self {
        self.action.inputs.insert(
            name.into(),
            Input {
                path: path.into(),
                source: InputSource::Output {
                    action: action_id.into(),
                    name: output_name.into(),
                },
            },
        );
        self
    }

    /// Declare an expected output at `path` (relative to the working directory).
    pub fn output(mut self, name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        self.action.outputs.insert(name.into(), path.into());
        self
    }

    /// Declare an additional environment variable (enters the cache key).
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.action.env.insert(key.into(), value.into());
        self
    }

    /// Set the working directory (relative to the sandbox root).
    pub fn working_directory(mut self, dir: impl Into<PathBuf>) -> Self {
        self.action.working_directory = dir.into();
        self
    }

    pub fn mode(mut self, mode: ExecutionMode) -> Self {
        self.action.execution_mode = mode;
        self
    }

    pub fn cache_policy(mut self, policy: CachePolicy) -> Self {
        self.action.cache_policy = policy;
        self
    }

    /// Set the configuration and the axes whose values this action consumes.
    pub fn configured(mut self, config: Configuration, consumed_axes: Vec<Axis>) -> Self {
        self.action.config = config;
        self.action.consumed_axes = consumed_axes;
        self
    }

    pub fn timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.action.timeout_ms = timeout_ms;
        self
    }

    /// Mark the action's output as independent of the target platform, so its cache
    /// key is shared across all platforms (§6.3). Combined with consuming no axes,
    /// this makes the action fully configuration-invariant.
    pub fn platform_independent(mut self) -> Self {
        self.action.platform_sensitive = false;
        self
    }

    /// Use snapshot-based caching: `paths` are the mutable cache directories
    /// (relative to the working directory) snapshotted under the coarse `key`. The
    /// action restores **and saves** the snapshot, and is action-cacheable. Sets the
    /// cache policy to [`CachePolicy::SnapshotBased`]. The snapshot is **shared** —
    /// consumers (`SnapshotConsuming`) may restore it, so it is saved to the CAS every
    /// build (e.g. pnpm's `node_modules`).
    pub fn snapshot(mut self, key: Digest, paths: Vec<PathBuf>) -> Self {
        self.action.snapshot_key = Some(key);
        self.action.snapshot_paths = paths;
        self.action.cache_policy = CachePolicy::SnapshotBased;
        self.action.snapshot_shared = true;
        self
    }

    /// Like [`ActionBuilder::snapshot`], but the snapshot is **private** — the owner's
    /// internal incremental state that **no action consumes** (e.g. cargo's `target/`).
    /// It is *not* saved to the CAS per build: with warm-sandbox reuse the in-place tree
    /// is the live copy, so the per-build save would be pure O(`target/`) overhead
    /// (`docs/sandboxing.md` §5.8.1). The action is still `SnapshotBased` (restorable on a
    /// cold start / eviction-recovery); only the per-build *save* is suppressed.
    pub fn snapshot_private(mut self, key: Digest, paths: Vec<PathBuf>) -> Self {
        self.action.snapshot_key = Some(key);
        self.action.snapshot_paths = paths;
        self.action.cache_policy = CachePolicy::SnapshotBased;
        self.action.snapshot_shared = false;
        self
    }

    /// **Restore** the snapshot at `key` (its `paths`) before running, **without
    /// saving** it back and **without** action-caching the result. For an action that
    /// consumes a snapshot another action owns (e.g. a pnpm script reading `install`'s
    /// `node_modules`) but whose own output is not trusted reproducible. Sets the cache
    /// policy to [`CachePolicy::SnapshotConsuming`].
    pub fn snapshot_restore(mut self, key: Digest, paths: Vec<PathBuf>) -> Self {
        self.action.snapshot_key = Some(key);
        self.action.snapshot_paths = paths;
        self.action.cache_policy = CachePolicy::SnapshotConsuming;
        self
    }

    pub fn build(self) -> Action {
        self.action
    }
}
