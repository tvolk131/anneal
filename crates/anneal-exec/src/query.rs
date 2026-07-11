//! Tool queries (DESIGN.md §3.6, spike): a sandboxed, keyed, cached action whose
//! **output is its captured stdout**, for facts only a tool can produce
//! (`cargo metadata`, `go list`, import closures).
//!
//! A [`QuerySpec`] is deliberately narrower than [`Action`] — the narrowness is
//! enforced by construction, not convention (§3.6 "pillar 3 applied to the
//! framework's own mechanism"):
//!
//! - **Sealed** execution and **network denied**, always. The builder exposes no
//!   way to change either.
//! - **No declared outputs**: the output is stdout, captured by the executor and
//!   stored in the CAS.
//! - **No snapshot/persistent state** (the phase-separated `Read` rung of the
//!   design arrives with the state-taxonomy work; the bootstrap query — the
//!   ladder's ground — needs none, which this spike demonstrates).
//! - **Deterministic cache policy**: identical query keys must produce identical
//!   stdout bytes. This is the §3.6 keystone (early cutoff at the analysis
//!   boundary), and it is what the stable sandbox root below exists to protect.
//!
//! ## Sandbox-root stability (the spike's second pressure point)
//!
//! Tools embed absolute paths in their stdout (`cargo metadata` reports
//! `workspace_root`, `manifest_path`, `target_directory`). Byte-determinism
//! therefore requires the sandbox path a query runs at to be **stable across
//! runs whose output should converge** — in particular across *input edits that
//! leave the output identical*, which is exactly the early-cutoff case. So the
//! query sandbox root is derived from the query's [`identity`](query_identity)
//! — command, env, toolchains, working directory — and deliberately **not** from
//! its input digests (a per-key root would bake the input digest into the
//! emitted paths and kill cutoff on every edit).
//!
//! Platform asymmetry, recorded: on Linux the sandbox binds the root at a fixed
//! guest path (`/work`), so emitted paths are machine-independent and query
//! outputs can converge across machines. On macOS there is no mount namespace;
//! the *host* path leaks into the output, so query outputs are stable
//! per-checkout but not across machines — consistent with §2.8, where
//! `LoudBestEffort` hosts never produce into shared caches anyway.

use std::path::PathBuf;

use anneal_core::Digest;

use crate::action::{Action, ActionBuilder, ActionError, Toolchain};
use crate::cache::action_digest;

/// The logical output name under which captured stdout is stored in the action
/// cache. Namespaced so a query entry can never be confused with a declared
/// file output.
pub(crate) const QUERY_STDOUT: &str = "__query_stdout";

/// A validated tool query. Construct via [`QuerySpec::builder`]; the builder is
/// the narrowing — there is no way to obtain a `QuerySpec` whose action has
/// outputs, network, snapshots, or a non-sealed mode.
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

    pub(crate) fn action(&self) -> &Action {
        &self.action
    }
}

/// Builder for [`QuerySpec`]. Exposes inputs, env, toolchains, working
/// directory, and timeout — nothing else. Compare [`ActionBuilder`], which this
/// wraps: every method *not* re-exported here is a capability queries don't get.
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
    /// network-denied + output-less by construction: those are `ActionBuilder`
    /// defaults, and this builder exposes no method that changes them.
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

/// The cache key for a query: the full action digest (inputs included),
/// namespaced so query entries and ordinary action entries can never collide.
pub(crate) fn query_key(action: &Action) -> Digest {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"anneal-query-v1\n");
    buf.extend_from_slice(action_digest(action).as_bytes());
    Digest::of(&buf)
}

/// The query's *identity*: everything that defines which query this is —
/// command, env, toolchain identities, working directory — and **nothing that
/// changes when its inputs change**. This keys the stable sandbox root (see the
/// module docs for why input digests must stay out of it). Two runs of the same
/// query over edited inputs land at the same path, so tool-emitted absolute
/// paths are byte-stable and early cutoff survives.
pub(crate) fn query_identity(action: &Action) -> Digest {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"anneal-query-identity-v1\n");
    for arg in &action.command {
        buf.extend_from_slice(arg.as_bytes());
        buf.push(0);
    }
    for (k, v) in &action.env {
        buf.extend_from_slice(k.as_bytes());
        buf.push(0);
        buf.extend_from_slice(v.as_bytes());
        buf.push(0);
    }
    for tc in action.toolchains.values() {
        buf.extend_from_slice(tc.identity().as_bytes());
        buf.push(0);
    }
    buf.extend_from_slice(action.working_directory().as_os_str().as_encoded_bytes());
    Digest::of(&buf)
}
