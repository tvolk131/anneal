//! The action specification (§19.1) and its construction.
//!
//! An [`Action`] is a pure description of work: a command, its declared inputs and
//! outputs, its environment, and the configuration it runs under. It is built only
//! through [`Action::builder`] so the defaults (sealed, deterministic, host config)
//! are applied consistently. Fields are `pub(crate)` — readable by the cache,
//! materializer, and sandbox modules, but constructible from outside only via the
//! builder.
//!
//! The rule-author-facing sealed execution contract is documented in
//! `docs/sandbox-contract.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

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
    /// Whether the action may mutate this materialized input as private scratch.
    ///
    /// The original digest still enters the action key; this only changes placement:
    /// the materializer uses a distinct writable copy rather than a CAS hardlink/clone,
    /// and the Linux sandbox does not overmount this path read-only.
    pub writable: bool,
    /// Whether this input is a **generated file the inner tool reads at a tree-shaped
    /// path** that `anneal materialize` should mirror into the developer's working tree
    /// (so native tools — `cargo run`, rust-analyzer — see what the sandbox sees). The
    /// analyzer derives a target's routed-data view from the inputs carrying this flag;
    /// there is no separate `Analysis.routed_data` field. It marks the rule's declaration
    /// that this edge is contract-visible generated data, NOT sandbox plumbing (a fetched
    /// `.crate`, a vendored tree) — a distinction the engine cannot infer structurally.
    ///
    /// It is a **materialize affordance, not build identity**: deliberately EXCLUDED from
    /// the action cache key (`cache.rs::action_digest` writes inputs field-by-field and
    /// never folds this in — like `writable` is folded but this is not). Two actions
    /// differing only in `mirror_to_tree` MUST hash identically.
    pub mirror_to_tree: bool,
}

/// A host toolchain made visible to an action.
///
/// The sandbox backend owns enforcement (read-only mounts, PATH shaping, closure
/// visibility), but the action model owns identity: changing the resolved toolchain
/// must change the action key. First-party rules currently require these paths to
/// resolve into `/nix/store/...`; the core type intentionally only records the
/// resolved identity and mount hints. See `docs/sandbox-contract.md` for the
/// toolchain availability and identity contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolchain {
    name: String,
    identity: String,
    bin_dirs: Vec<PathBuf>,
    read_only_roots: Vec<PathBuf>,
}

impl Toolchain {
    pub fn new(
        name: impl Into<String>,
        identity: impl Into<String>,
        bin_dirs: Vec<PathBuf>,
        read_only_roots: Vec<PathBuf>,
    ) -> Result<Self, ActionError> {
        let toolchain = Toolchain {
            name: name.into(),
            identity: identity.into(),
            bin_dirs,
            read_only_roots,
        };
        toolchain.validate()?;
        Ok(toolchain)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn bin_dirs(&self) -> &[PathBuf] {
        &self.bin_dirs
    }

    pub fn read_only_roots(&self) -> &[PathBuf] {
        &self.read_only_roots
    }

    fn validate(&self) -> Result<(), ActionError> {
        validate_logical_name("toolchain name", &self.name)?;
        if self.identity.is_empty() {
            return Err(ActionError::new(format!(
                "toolchain {:?} has an empty identity",
                self.name
            )));
        }
        for path in &self.bin_dirs {
            validate_host_path("toolchain bin dir", path)?;
        }
        for path in &self.read_only_roots {
            validate_host_path("toolchain read-only root", path)?;
        }
        Ok(())
    }
}

/// A malformed action contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionError {
    message: String,
}

impl ActionError {
    fn new(message: impl Into<String>) -> Self {
        ActionError {
            message: message.into(),
        }
    }
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ActionError {}

/// Isolation level for a running action (§7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionMode {
    /// Hermetic execution boundary. The default and the only cacheable mode.
    ///
    /// Linux sealed actions provide strict filesystem visibility through
    /// `bubblewrap`; macOS sealed actions provide a Seatbelt filesystem/network
    /// policy, but not Linux-style namespace isolation. See
    /// `docs/sandbox-contract.md`.
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
    /// **Fixed-output** (a Nix fixed-output derivation; Bazel `http_archive(sha256)`):
    /// the action's single declared output is pinned to `expected` *before* it runs.
    /// That a-priori knowledge is what licenses the network — the action is cached by
    /// its **output** (`cas.has(expected)`), not its inputs, so an already-present blob
    /// skips the fetch entirely, and a produced output is verified byte-for-byte against
    /// `expected` (a mismatch fails closed). The acquisition layer for hash-pinned deps,
    /// toolchains, and archives (`docs/...` §FOD). Built via [`ActionBuilder::fixed_output`].
    FixedOutput { expected: Digest },
}

impl CachePolicy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CachePolicy::Deterministic => "deterministic",
            CachePolicy::NonCacheable => "non_cacheable",
            CachePolicy::SnapshotBased => "snapshot_based",
            CachePolicy::SnapshotConsuming => "snapshot_consuming",
            CachePolicy::FixedOutput { .. } => "fixed_output",
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
    /// Toolchains whose resolved identity and read-only roots are part of the action
    /// contract. The Linux sandbox will mount these roots read-only.
    pub(crate) toolchains: BTreeMap<String, Toolchain>,
    /// Working directory relative to the sandbox root (default ".").
    pub(crate) working_directory: PathBuf,
    pub(crate) execution_mode: ExecutionMode,
    pub(crate) cache_policy: CachePolicy,
    /// The configuration this action runs under (§3.3).
    pub(crate) config: Configuration,
    /// Which axes this action's cache key depends on — drives trimming (§6.2).
    pub(crate) consumed_axes: BTreeSet<Axis>,
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
    /// Whether the action is permitted network access. Default `false` — the sealed-build
    /// goal (§7) is no network. Set `true` only for fixed-output fetches (where the output
    /// hash fences the impurity) and the `exec` escape hatch. Kept orthogonal to
    /// [`CachePolicy`] so the capability is reusable; Linux sealed actions enforce the
    /// default with a private network namespace, and macOS sealed actions enforce it via
    /// `sandbox-exec`. (A **native** fetch — `fetch_url` set — does its I/O in the
    /// executor process, so no sandbox enforcement applies; the flag stays set as the
    /// honest capability declaration.)
    pub(crate) network: bool,
    /// For a **native fixed-output fetch** (§FOD): the URL the executor
    /// downloads in-process (pure-Rust TLS, Mozilla roots compiled in) instead
    /// of spawning a sandboxed command. Set via [`ActionBuilder::fetch`];
    /// always paired with [`CachePolicy::FixedOutput`] and an empty `command`
    /// — there is no inner tool. The pin carries the integrity guarantee, so
    /// the embedded root store is availability-only configuration.
    pub(crate) fetch_url: Option<String>,
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
                toolchains: BTreeMap::new(),
                working_directory: PathBuf::from("."),
                execution_mode: ExecutionMode::default(),
                cache_policy: CachePolicy::default(),
                config: default_host_config(),
                consumed_axes: BTreeSet::new(),
                timeout_ms: 600_000,
                snapshot_paths: Vec::new(),
                snapshot_key: None,
                snapshot_shared: true,
                platform_sensitive: true,
                network: false,
                fetch_url: None,
            },
        }
    }

    /// The action's name. Excluded from the cache key (§8.1), but it is the
    /// **graph-unique identity** other actions use to reference this action's
    /// outputs ([`InputSource::Output`]), so it must be unique within a graph.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Declared inputs, keyed by logical input name.
    pub fn inputs(&self) -> &BTreeMap<String, Input> {
        &self.inputs
    }

    /// Additional environment variables for this action (names and values both
    /// enter the cache key, §7.4).
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// Toolchains attached to this action, keyed by name. Their `read_only_roots`
    /// are mounted read-only in the sandbox and their identities enter the cache key.
    pub fn toolchains(&self) -> &BTreeMap<String, Toolchain> {
        &self.toolchains
    }

    /// Declared outputs, keyed by logical output name.
    pub fn outputs(&self) -> &BTreeMap<String, PathBuf> {
        &self.outputs
    }

    /// The action working directory relative to the sandbox root.
    /// The snapshot key, if this action uses a persistent state tree.
    pub fn snapshot_key(&self) -> Option<Digest> {
        self.snapshot_key
    }

    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    /// Whether the action is permitted network access (see the `network` field). The
    /// sandbox/executor consults this; enforcement of the `false` default is later
    /// hardening.
    pub fn allows_network(&self) -> bool {
        self.network
    }

    /// The URL of a native fixed-output fetch, if this action is one.
    pub fn fetch_url(&self) -> Option<&str> {
        self.fetch_url.as_deref()
    }

    /// Validate the action contract before materialization or keying. This is the
    /// central path-safety check: action paths are relative to the action working
    /// directory and must not contain absolute roots or parent components.
    pub fn validate(&self) -> Result<(), ActionError> {
        validate_action_name(&self.name)?;
        if self.fetch_url.is_some() {
            return self.validate_fetch_contract();
        }
        if self.command.is_empty() {
            return Err(ActionError::new(format!(
                "action {:?} has an empty command",
                self.name
            )));
        }
        self.validate_command_contract()?;
        validate_relative_path("working directory", &self.working_directory, true)?;

        // DESIGN.md §4.4: Hermetic is enforced, not conventional. A private
        // snapshot owner is a mutator of interleaved tool state (the typed
        // `mutate_state` grant lowers to exactly this shape), and Hermetic
        // means "no interleaved mutation." Shared snapshots (phase-separated
        // producers) and restores (consumers) remain legal — CI must populate
        // phase-separated state.
        if self.config.axes().exec_mode == anneal_core::ExecMode::Hermetic
            && matches!(self.cache_policy, CachePolicy::SnapshotBased)
            && !self.snapshot_shared
        {
            return Err(ActionError::new(format!(
                "action {:?} mutates interleaved state under ExecMode::Hermetic — \
                 hermetic actions may not take mutate_state grants (DESIGN.md §4.4); \
                 emit the warm variant only under Incremental",
                self.name
            )));
        }

        for (key, value) in &self.env {
            if key.is_empty() || key.contains('=') || key.contains('\0') {
                return Err(ActionError::new(format!(
                    "action {:?} has invalid environment variable name {:?}",
                    self.name, key
                )));
            }
            if value.contains('\0') {
                return Err(ActionError::new(format!(
                    "action {:?} environment variable {:?} contains a NUL byte",
                    self.name, key
                )));
            }
        }

        let mut input_paths = BTreeSet::new();
        for (name, input) in &self.inputs {
            validate_logical_name("input name", name)?;
            validate_relative_path("input path", &input.path, false)?;
            if !input_paths.insert(input.path.clone()) {
                return Err(ActionError::new(format!(
                    "action {:?} declares duplicate input path `{}`",
                    self.name,
                    input.path.display()
                )));
            }
        }

        let mut output_paths = BTreeSet::new();
        for (name, path) in &self.outputs {
            validate_logical_name("output name", name)?;
            validate_relative_path("output path", path, false)?;
            if !output_paths.insert(path.clone()) {
                return Err(ActionError::new(format!(
                    "action {:?} declares duplicate output path `{}`",
                    self.name,
                    path.display()
                )));
            }
            if input_paths.contains(path) {
                return Err(ActionError::new(format!(
                    "action {:?} declares `{}` as both input and output",
                    self.name,
                    path.display()
                )));
            }
        }

        let mut snapshot_paths = BTreeSet::new();
        for path in &self.snapshot_paths {
            validate_relative_path("snapshot path", path, false)?;
            if !snapshot_paths.insert(path.clone()) {
                return Err(ActionError::new(format!(
                    "action {:?} declares duplicate snapshot path `{}`",
                    self.name,
                    path.display()
                )));
            }
        }
        for input in &input_paths {
            if snapshot_paths
                .iter()
                .any(|snapshot| input == snapshot || input.starts_with(snapshot))
            {
                return Err(ActionError::new(format!(
                    "action {:?} declares input `{}` inside a mutable snapshot path",
                    self.name,
                    input.display()
                )));
            }
        }

        for (name, toolchain) in &self.toolchains {
            if name != toolchain.name() {
                return Err(ActionError::new(format!(
                    "action {:?} stores toolchain {:?} under mismatched key {:?}",
                    self.name,
                    toolchain.name(),
                    name
                )));
            }
            toolchain.validate()?;
        }

        Ok(())
    }

    /// A native fetch runs no inner tool: nothing that only matters inside a
    /// sandbox may be declared, and the §FOD single-pin shape is enforced at
    /// build time rather than first execution.
    fn validate_fetch_contract(&self) -> Result<(), ActionError> {
        let problem = if !self.command.is_empty() {
            Some("declares a command (a native fetch runs no inner tool)")
        } else if !self.inputs.is_empty() {
            Some("declares inputs (a native fetch has none)")
        } else if !self.toolchains.is_empty() {
            Some("declares toolchains (a native fetch consults none)")
        } else if !self.snapshot_paths.is_empty() {
            Some("declares snapshot paths (a native fetch has no tool state)")
        } else if self.outputs.len() != 1 {
            Some("must declare exactly one output (the pin is a single digest)")
        } else if !matches!(self.cache_policy, CachePolicy::FixedOutput { .. }) {
            Some("must be FixedOutput (use ActionBuilder::fetch)")
        } else {
            None
        };
        match problem {
            Some(problem) => Err(ActionError::new(format!(
                "native fetch action {:?} {problem}",
                self.name
            ))),
            None => Ok(()),
        }
    }

    fn validate_command_contract(&self) -> Result<(), ActionError> {
        if self.execution_mode != ExecutionMode::Sealed {
            return Ok(());
        }

        let program = Path::new(&self.command[0]);
        if program.is_absolute() {
            if self
                .toolchains
                .values()
                .flat_map(Toolchain::read_only_roots)
                .any(|root| program.starts_with(root))
            {
                return Ok(());
            }
            return Err(ActionError::new(format!(
                "sealed action {:?} command `{}` is an absolute host path outside declared toolchain roots",
                self.name,
                program.display()
            )));
        }

        if self.command[0].contains('/') {
            validate_executable_path("sealed action command", program)?;
            return Ok(());
        }

        if self.toolchains.is_empty() {
            return Err(ActionError::new(format!(
                "sealed action {:?} command {:?} is resolved from PATH but declares no toolchain/runtime",
                self.name, self.command[0]
            )));
        }
        let Some(path) = self.env.get("PATH") else {
            return Err(ActionError::new(format!(
                "sealed action {:?} command {:?} is resolved from PATH but the action does not declare PATH",
                self.name, self.command[0]
            )));
        };
        validate_path_env(&self.name, path, &self.toolchains)
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
    /// Whether a snapshot (persistent state tree) is already attached. The
    /// action model carries at most one; the typed state layer in
    /// `anneal-rules` uses this to reject a second grant at analysis time.
    pub fn snapshot_is_set(&self) -> bool {
        self.action.snapshot_key.is_some()
    }

    /// Declare an input from a concrete CAS blob: materialize `digest` at `path`
    /// (relative to the working directory) under the logical `name`.
    pub fn input(
        mut self,
        name: impl Into<String>,
        path: impl Into<PathBuf>,
        digest: Digest,
    ) -> Self {
        self.action.inputs.insert(
            name.into(),
            Input {
                path: path.into(),
                source: InputSource::Blob(digest),
                writable: false,
                mirror_to_tree: false,
            },
        );
        self
    }

    /// Declare an input from a concrete CAS blob that the action may mutate privately.
    ///
    /// Use this for tools that rewrite input manifests in-place as part of otherwise
    /// deterministic operation (for example pnpm's atomic lockfile refresh). The input
    /// digest remains part of the action key; mutations are not captured unless the path
    /// is separately declared as an output.
    pub fn writable_input(
        mut self,
        name: impl Into<String>,
        path: impl Into<PathBuf>,
        digest: Digest,
    ) -> Self {
        self.action.inputs.insert(
            name.into(),
            Input {
                path: path.into(),
                source: InputSource::Blob(digest),
                writable: true,
                mirror_to_tree: false,
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
                writable: false,
                mirror_to_tree: false,
            },
        );
        self
    }

    /// Declare an input from another action's output that the action may mutate privately.
    pub fn writable_input_from_output(
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
                writable: true,
                mirror_to_tree: false,
            },
        );
        self
    }

    /// Declare an input from another action's output that is **dev-tree-visible
    /// generated data** (`mirror_to_tree`): identical to [`input_from_output`] but flags
    /// the input so `anneal materialize` mirrors it into the working tree. Use this for a
    /// rule's consumed `data`/routed edges (a generated `config.json`, a routed file) —
    /// NOT for sandbox plumbing (a fetched `.crate`, an internal test binary), which use
    /// the plain [`input_from_output`].
    ///
    /// [`input_from_output`]: Self::input_from_output
    pub fn routed_input_from_output(
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
                writable: false,
                mirror_to_tree: true,
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

    /// Declare a toolchain dependency. Its identity enters the action key and its
    /// read-only roots are mount hints for sandbox backends.
    pub fn toolchain(mut self, toolchain: Toolchain) -> Self {
        self.action
            .toolchains
            .insert(toolchain.name().to_owned(), toolchain);
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
    pub fn configured(
        mut self,
        config: Configuration,
        consumed_axes: impl IntoIterator<Item = Axis>,
    ) -> Self {
        self.action.config = config;
        self.action.consumed_axes = consumed_axes.into_iter().collect();
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

    /// Permit (or forbid) network access for this action (default forbidden). Orthogonal
    /// to the cache policy — for the `exec` escape hatch and as the capability
    /// [`ActionBuilder::fixed_output`] turns on.
    pub fn network(mut self, enabled: bool) -> Self {
        self.action.network = enabled;
        self
    }

    /// Make this a **fixed-output** fetch: its single declared `output` is pinned to
    /// `expected`. Sets [`CachePolicy::FixedOutput`] and **enables the network** (the
    /// pin fences the impurity). The action is cached by output content — an already-
    /// present `expected` blob skips the fetch — and any produced output is verified
    /// against `expected`, failing closed on a mismatch. Declare exactly one output.
    pub fn fixed_output(mut self, expected: Digest) -> Self {
        self.action.cache_policy = CachePolicy::FixedOutput { expected };
        self.action.network = true;
        self
    }

    /// Declare a **native fixed-output fetch** (§FOD): the executor downloads
    /// `url` in-process — pure-Rust TLS with Mozilla's root store compiled in
    /// — and verifies the bytes against `expected` before admitting them to
    /// the CAS. No sandbox is spawned and no toolchain is consulted, so the
    /// action must carry an empty `command`, no inputs, no toolchains, and
    /// exactly one output (validated by [`try_build`]). The `network`
    /// capability is set for honesty: this action reaches the network, just
    /// from the executor process rather than a sandbox.
    ///
    /// [`try_build`]: ActionBuilder::try_build
    pub fn fetch(mut self, url: impl Into<String>, expected: Digest) -> Self {
        self.action.fetch_url = Some(url.into());
        self.action.cache_policy = CachePolicy::FixedOutput { expected };
        self.action.network = true;
        self
    }

    pub fn try_build(self) -> Result<Action, ActionError> {
        self.action.validate()?;
        Ok(self.action)
    }

    pub fn build(self) -> Action {
        self.try_build().expect("invalid action")
    }
}

fn validate_logical_name(kind: &str, name: &str) -> Result<(), ActionError> {
    if name.is_empty() || name.chars().any(char::is_whitespace) || name.contains('\0') {
        return Err(ActionError::new(format!("{kind} {name:?} is invalid")));
    }
    Ok(())
}

fn validate_action_name(name: &str) -> Result<(), ActionError> {
    if name.is_empty() || name.contains('\0') {
        return Err(ActionError::new(format!("action name {name:?} is invalid")));
    }
    Ok(())
}

fn validate_relative_path(kind: &str, path: &Path, allow_dot: bool) -> Result<(), ActionError> {
    if path.as_os_str().is_empty() {
        return Err(ActionError::new(format!("{kind} must not be empty")));
    }
    if path == Path::new(".") {
        return if allow_dot {
            Ok(())
        } else {
            Err(ActionError::new(format!("{kind} `.` is not allowed")))
        };
    }
    if path.is_absolute() {
        return Err(ActionError::new(format!(
            "{kind} `{}` must be relative",
            path.display()
        )));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {
                return Err(ActionError::new(format!(
                    "{kind} `{}` must not contain `.` components",
                    path.display()
                )));
            }
            Component::ParentDir => {
                return Err(ActionError::new(format!(
                    "{kind} `{}` must not contain `..` components",
                    path.display()
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ActionError::new(format!(
                    "{kind} `{}` must not contain a root or drive prefix",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn validate_executable_path(kind: &str, path: &Path) -> Result<(), ActionError> {
    if path.as_os_str().is_empty() {
        return Err(ActionError::new(format!("{kind} must not be empty")));
    }
    if path.is_absolute() {
        return Err(ActionError::new(format!(
            "{kind} `{}` must be relative or declared as a toolchain root",
            path.display()
        )));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(ActionError::new(format!(
                    "{kind} `{}` must not contain `..` components",
                    path.display()
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ActionError::new(format!(
                    "{kind} `{}` must not contain a root or drive prefix",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn validate_path_env(
    action_name: &str,
    path: &str,
    toolchains: &BTreeMap<String, Toolchain>,
) -> Result<(), ActionError> {
    if path.is_empty() {
        return Err(ActionError::new(format!(
            "sealed action {action_name:?} declares an empty PATH"
        )));
    }
    for entry in std::env::split_paths(path) {
        if entry.as_os_str().is_empty() {
            return Err(ActionError::new(format!(
                "sealed action {action_name:?} declares an empty PATH entry"
            )));
        }
        if entry.is_absolute() {
            if toolchains
                .values()
                .flat_map(Toolchain::read_only_roots)
                .any(|root| entry.starts_with(root))
            {
                continue;
            }
            return Err(ActionError::new(format!(
                "sealed action {action_name:?} PATH entry `{}` is outside declared toolchain roots",
                entry.display()
            )));
        }
        validate_executable_path("sealed action PATH entry", &entry)?;
    }
    Ok(())
}

fn validate_host_path(kind: &str, path: &Path) -> Result<(), ActionError> {
    if path.as_os_str().is_empty() {
        return Err(ActionError::new(format!("{kind} must not be empty")));
    }
    if !path.is_absolute() {
        return Err(ActionError::new(format!(
            "{kind} `{}` must be absolute",
            path.display()
        )));
    }
    for component in path.components() {
        match component {
            Component::RootDir | Component::Normal(_) => {}
            Component::CurDir => {
                return Err(ActionError::new(format!(
                    "{kind} `{}` must not contain `.` components",
                    path.display()
                )));
            }
            Component::ParentDir => {
                return Err(ActionError::new(format!(
                    "{kind} `{}` must not contain `..` components",
                    path.display()
                )));
            }
            Component::Prefix(_) => {
                return Err(ActionError::new(format!(
                    "{kind} `{}` must not contain a drive prefix",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> Toolchain {
        Toolchain::new(
            "runtime",
            "runtime-id",
            vec![PathBuf::from("/nix/store/runtime/bin")],
            vec![PathBuf::from("/nix/store/runtime")],
        )
        .unwrap()
    }

    #[test]
    fn rejects_paths_that_can_escape_the_sandbox() {
        let bad_abs = Action::builder("a", ["./tool"])
            .input("in", "/tmp/in", Digest::of(b"in"))
            .try_build()
            .unwrap_err();
        assert!(bad_abs.to_string().contains("must be relative"));

        let bad_parent = Action::builder("a", ["./tool"])
            .output("out", "../out")
            .try_build()
            .unwrap_err();
        assert!(bad_parent.to_string().contains("must not contain `..`"));
    }

    #[test]
    fn hermetic_rejects_interleaved_mutation() {
        use anneal_core::{AxisValues, Configuration, ExecMode, Platform};
        let hermetic = Configuration::new(
            Platform::new("host", "host"),
            AxisValues {
                exec_mode: ExecMode::Hermetic,
                ..Default::default()
            },
        );

        // A private snapshot owner is a mutate_state grant: rejected (§4.4).
        let err = Action::builder("a", ["./tool"])
            .configured(hermetic.clone(), vec![])
            .snapshot_private(Digest::of(b"k"), vec!["target".into()])
            .try_build()
            .unwrap_err();
        assert!(err.to_string().contains("Hermetic"));

        // Shared snapshots (phase-separated producers) and restores
        // (consumers) remain legal: Hermetic forbids mutation, not state.
        assert!(Action::builder("a", ["./tool"])
            .configured(hermetic.clone(), vec![])
            .snapshot(Digest::of(b"k"), vec!["node_modules".into()])
            .try_build()
            .is_ok());
        assert!(Action::builder("a", ["./tool"])
            .configured(hermetic, vec![])
            .snapshot_restore(Digest::of(b"k"), vec!["node_modules".into()])
            .try_build()
            .is_ok());

        // And the same mutator is fine under the default (Incremental) config.
        assert!(Action::builder("a", ["./tool"])
            .snapshot_private(Digest::of(b"k"), vec!["target".into()])
            .try_build()
            .is_ok());
    }

    #[test]
    fn rejects_input_output_path_collision() {
        let err = Action::builder("a", ["./tool"])
            .input("in", "same", Digest::of(b"in"))
            .output("out", "same")
            .try_build()
            .unwrap_err();
        assert!(err.to_string().contains("both input and output"));
    }

    #[test]
    fn validates_toolchain_paths() {
        Action::builder("a", ["/nix/store/runtime/bin/true"])
            .toolchain(runtime())
            .try_build()
            .unwrap();

        let err = Toolchain::new(
            "rust",
            "id",
            vec![PathBuf::from("relative/bin")],
            Vec::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("must be absolute"));
    }

    #[test]
    fn sealed_relative_executable_is_self_contained() {
        Action::builder("a", ["./tool"]).try_build().unwrap();
        Action::builder("a", ["bin/tool"]).try_build().unwrap();

        let err = Action::builder("a", ["../tool"]).try_build().unwrap_err();
        assert!(err.to_string().contains("must not contain `..`"));
    }

    #[test]
    fn sealed_bare_command_requires_declared_runtime_and_path() {
        let no_toolchain = Action::builder("a", ["sh"]).try_build().unwrap_err();
        assert!(no_toolchain.to_string().contains("declares no toolchain"));

        let no_path = Action::builder("a", ["sh"])
            .toolchain(runtime())
            .try_build()
            .unwrap_err();
        assert!(no_path.to_string().contains("does not declare PATH"));

        Action::builder("a", ["sh"])
            .toolchain(runtime())
            .env("PATH", "/nix/store/runtime/bin")
            .try_build()
            .unwrap();
    }

    #[test]
    fn sealed_absolute_command_and_path_must_be_declared() {
        let host_command = Action::builder("a", ["/bin/sh"]).try_build().unwrap_err();
        assert!(host_command
            .to_string()
            .contains("outside declared toolchain roots"));

        let host_path = Action::builder("a", ["sh"])
            .toolchain(runtime())
            .env("PATH", "/bin")
            .try_build()
            .unwrap_err();
        assert!(host_path
            .to_string()
            .contains("outside declared toolchain roots"));
    }
}
