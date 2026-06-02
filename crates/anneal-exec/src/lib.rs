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
//! ## Milestone 1 scope
//!
//! Local execution only ([`LocalExecutor`]); `Executor` is a trait so a future
//! `RemoteExecutor` slots in without changing callers (§7.1). `snapshot_based`
//! caching (§8.2) is Phase 3 (`anneal-snapshot`); persistent workers (§10) and
//! `platform_requirements` are v1.x. macOS `sealed`-mode filesystem isolation is
//! best-effort (§7.3, §22) — environment scrubbing is enforced; network is denied
//! via `sandbox-exec`; strict input-only filesystem visibility is deferred.

mod action;
mod cache;
mod executor;
mod materializer;
mod sandbox;
mod verify;
// The warm-sandbox sync engine (docs/sandboxing.md §5). Landed and unit-tested on its
// own; the executor wiring (warm-dir lifecycle, commit record, fallback) is the next
// increment, at which point this `allow` comes off.
#[allow(dead_code)]
mod warm;

pub use action::{Action, ActionBuilder, CachePolicy, ExecutionMode, Input, InputSource};
pub use cache::action_digest;
pub use executor::{ActionResult, ExecError, Executor, LocalExecutor, PhaseTimings};
pub use verify::{prime_snapshot, verify_correctness_neutral, NeutralityReport};

/// Participates in every cache key (§8.1). Bump when sandbox semantics change so that
/// a sandbox behavior change invalidates previously-cached results.
pub(crate) const SANDBOX_VERSION: &str = "anneal-sandbox-1";
