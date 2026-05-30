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

/// A declared input: a CAS blob to be materialized at `path` (relative to the
/// action's working directory) inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub path: PathBuf,
    pub digest: Digest,
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
    /// Direct execution with no isolation (used by `mybuild exec`). Not cacheable.
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
    /// Cached with a stateful snapshot (§8.2). Implemented in Phase 3
    /// (`anneal-snapshot`); treated as non-cacheable by the kernel until then.
    SnapshotBased,
}

impl CachePolicy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CachePolicy::Deterministic => "deterministic",
            CachePolicy::NonCacheable => "non_cacheable",
            CachePolicy::SnapshotBased => "snapshot_based",
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
            },
        }
    }

    /// The action's human-facing name (excluded from the cache key, §8.1).
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
    /// Declare an input: materialize the CAS blob `digest` at `path` (relative to the
    /// working directory) under the logical `name`.
    pub fn input(mut self, name: impl Into<String>, path: impl Into<PathBuf>, digest: Digest) -> Self {
        self.action.inputs.insert(
            name.into(),
            Input {
                path: path.into(),
                digest,
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

    pub fn build(self) -> Action {
        self.action
    }
}
