//! `anneal-exec` — the execution kernel (§7).
//!
//! A deep module. Its public surface is essentially one method —
//! [`Executor::execute`] — which turns an [`Action`] into an [`ActionResult`].
//! Everything about *how* an action runs is hidden behind that interface, split
//! into four private concerns (the layering from the design doc):
//!
//! | module        | concern                | answers                         |
//! |---------------|------------------------|---------------------------------|
//! | [`action`]    | the action spec (§19.1) + cache-key | *what* is being run         |
//! | [`cache`]     | action cache (§8.1)    | *have we already run this?*     |
//! | [`materializer`] | CAS ↔ filesystem (§3.4) | *where do the bytes go?*     |
//! | [`sandbox`]   | OS isolation (§7.3)    | *what is the action allowed to do?* |
//!
//! The orchestration that ties them together lives in [`executor`]. A caller of
//! `execute` never names the sandbox or the materializer; the only knob that reaches
//! them is the action's `execution_mode` field — data on the action, not an API.
//!
//! ## Scope
//!
//! Local execution only ([`LocalExecutor`]); `Executor` is a trait so a future
//! `RemoteExecutor` slots in without changing callers (§7.1). Linux `sealed` mode
//! uses `bubblewrap` for strict filesystem visibility and default network denial.
//! macOS `sealed` mode uses `sandbox-exec` for a Seatbelt filesystem/network
//! policy, but strict Linux-style namespace hermeticity still requires running on
//! Linux.
//! The precise sealed-mode contract lives in `docs/sandbox-contract.md`.

mod action;
mod cache;
mod executor;
/// Materializing provided files into the working tree (`anneal materialize`):
/// the manifest-tracked bridge from CAS outputs to what native tools (cargo
/// run, rust-analyzer) can see. Not part of the [`Executor`] deep module — a
/// user-facing surface of its own, so it stays a public module rather than
/// flat re-exports. (Distinct from the private `materializer`, which stages
/// action *inputs* into sandboxes.)
pub mod materialize;
mod materializer;
/// Tool queries (DESIGN.md §3.6, spiked): sealed, network-denied, stdout-captured
/// actions whose output feeds analysis. See the module docs for the sandbox-root
/// stability contract.
mod query;
mod sandbox;
/// Trust plumbing (DESIGN.md §2.4, §2.8): enforcement grades, computed cache
/// tiers, and cache-entry provenance.
mod trust;
mod verify;
/// The warm-sandbox sync engine (docs/sandboxing.md §5), wired into the executor's
/// snapshot-owner path via `LocalExecutor::warm_reuse`.
mod warm;

pub use action::{
    Action, ActionBuilder, ActionError, CachePolicy, ExecutionMode, Input, InputSource, Toolchain,
};
pub use cache::action_digest;
pub use executor::{ActionResult, ExecError, Executor, LocalExecutor, PhaseTimings, SandboxError};
pub use query::{QueryBuilder, QueryResult, QuerySpec};
pub use trust::{compute_tier, CacheTier, EnforcementGrade, Provenance};
pub use verify::{
    prime_snapshot, verify_correctness_neutral, verify_warm_neutral, NeutralityReport,
};

/// Participates in every cache key (§8.1). Bump when sandbox semantics change so that
/// a sandbox behavior change invalidates previously-cached results.
pub(crate) const SANDBOX_VERSION: &str = "anneal-sandbox-7";
