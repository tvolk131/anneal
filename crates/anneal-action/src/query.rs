//! Tool queries (DESIGN.md §3.6): a sandboxed, keyed, cached action whose
//! **output is its captured stdout**, for facts only a tool can produce
//! (`cargo metadata`, `go list`, import closures).
//!
//! A [`QuerySpec`] is deliberately narrower than [`Action`] — the narrowness
//! is enforced by construction, not convention (§3.6 "pillar one applied to
//! the framework's own mechanism"):
//!
//! - **Sealed** execution and **network denied**, always. The builder exposes
//!   no way to change either.
//! - **No declared outputs**: the output is stdout, captured by the engine and
//!   stored in the CAS.
//! - **No snapshot/persistent state**: the bootstrap query needs none.
//! - **Deterministic cache policy**: identical query keys must produce
//!   identical stdout bytes — the §3.6 keystone (early cutoff at the analysis
//!   boundary).
//!
//! This module owns the *contract*; the execution engine (`anneal-exec`)
//! implements [`QueryRunner`], owns the stable sandbox roots and the
//! namespaced cache keys, and is the only party that runs a query.
//!
//! ## Sandbox-root stability
//!
//! Tools embed absolute paths in their stdout (`cargo metadata` reports
//! `workspace_root`, `manifest_path`, `target_directory`). Byte-determinism
//! therefore requires the sandbox path a query runs at to be **stable across
//! runs whose output should converge** — in particular across *input edits
//! that leave the output identical*, which is exactly the early-cutoff case.
//! The engine derives that root from the query's identity (command, env,
//! toolchains, working directory) and deliberately **not** from its input
//! digests; see `anneal-exec`'s query module.

use std::path::PathBuf;

use anneal_core::Digest;

use crate::action::{Action, ActionBuilder, ActionError, Toolchain};

/// A validated tool query. Construct via [`QuerySpec::builder`]; the builder is
/// the narrowing — there is no way to obtain a `QuerySpec` whose action has
/// outputs, network, snapshots, or a non-sealed mode.
#[derive(Debug, Clone)]
pub struct QuerySpec {
    action: Action,
}

impl QuerySpec {
    /// Start building a query that runs `command` (argv; `command[0]` is the
    /// program, resolved via the declared toolchains).
    pub fn builder(name: impl Into<String>, command: Vec<String>) -> QueryBuilder {
        QueryBuilder {
            inner: Action::builder(name, command),
        }
    }

    /// The action this query lowers to. The engine's eyes; the narrowing above
    /// is enforced by [`QueryBuilder::build`], not by hiding this.
    pub fn action(&self) -> &Action {
        &self.action
    }
}

/// Builder for [`QuerySpec`]. Exposes inputs, env, toolchains, working
/// directory, and timeout — nothing else. Compare [`ActionBuilder`], which
/// this wraps: every method *not* re-exported here is a capability queries
/// don't get.
pub struct QueryBuilder {
    inner: ActionBuilder,
}

impl QueryBuilder {
    /// Declare an input blob at `path` (relative to the working directory).
    pub fn input(self, name: impl Into<String>, path: impl Into<PathBuf>, digest: Digest) -> Self {
        QueryBuilder {
            inner: self.inner.source_input(name, path, digest),
        }
    }

    /// Add an environment variable (exhaustive; nothing is inherited).
    pub fn env(self, key: impl Into<String>, value: impl Into<String>) -> Self {
        QueryBuilder {
            inner: self.inner.env(key, value),
        }
    }

    /// Mount a pinned toolchain (read-only) and resolve programs through it.
    pub fn toolchain(self, toolchain: Toolchain) -> Self {
        QueryBuilder {
            inner: self.inner.toolchain(toolchain),
        }
    }

    /// Set the working directory (relative to the sandbox root).
    pub fn working_directory(self, dir: impl Into<PathBuf>) -> Self {
        QueryBuilder {
            inner: self.inner.working_directory(dir),
        }
    }

    pub fn timeout_ms(self, timeout_ms: u64) -> Self {
        QueryBuilder {
            inner: self.inner.timeout_ms(timeout_ms),
        }
    }

    /// Validate and seal. The resulting action is Sealed + Deterministic +
    /// network-denied + output-less by construction: those are
    /// [`ActionBuilder`] defaults, and this builder exposes no method that
    /// changes them.
    pub fn build(self) -> Result<QuerySpec, ActionError> {
        let action = self.inner.try_build()?;
        debug_assert!(action.outputs().is_empty());
        debug_assert!(!action.allows_network());
        Ok(QuerySpec { action })
    }
}

/// The result of a query: captured stdout, and whether it came from the cache.
pub struct QueryResult {
    pub stdout: Vec<u8>,
    pub cache_hit: bool,
}

/// Why a query could not run. The engine's rich error is engine-side; the
/// contract carries the rendered diagnosis (rule errors stringify anyway).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError(pub String);

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for QueryError {}

/// Runs tool queries at analysis time. Implemented by the execution engine;
/// taken by the rules crate's `RuleContext` as its sole optional capability —
/// a query-free analysis run wires none, and rule tests can inject a fake
/// instead of a real engine.
pub trait QueryRunner {
    fn run_query(&self, spec: &QuerySpec) -> Result<QueryResult, QueryError>;
}
