//! `anneal-exec` — the execution kernel (§7).
//!
//! A deep module. Its public surface is essentially one method —
//! [`Executor::execute`] — which turns an [`Action`] into an [`ActionResult`].
//! Everything about *how* an action runs is hidden behind that interface. The
//! *what* — the action spec and its cache identity — lives in `anneal-action`;
//! the *have we already run this* — persisted results — lives in
//! `anneal-store`. This crate is the how, split into private concerns:
//!
//! | module        | concern                | answers                         |
//! |---------------|------------------------|---------------------------------|
//! | [`executor`]  | orchestration + parallel scheduling | *what runs when, and in what order?* |
//! | [`materializer`] | CAS ↔ filesystem (§3.4) | *where do the bytes go?*     |
//! | [`sandbox`]   | OS isolation (§7.3)    | *what is the action allowed to do?* |
//! | [`warm`]      | warm-tree reuse (§5)   | *how does native tool state survive?* |
//!
//! A caller of `execute` never names the sandbox or the materializer; the only
//! knob that reaches them is the action's `execution_mode` — data on the
//! action, not an API.
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

mod executor;
/// Native fixed-output downloads (§FOD): the executor fetches pinned blobs
/// in-process (rustls + embedded Mozilla roots) — no curl, no sandbox, no
/// host trust configuration. See the module docs for the trust argument.
mod fetch;
/// Materializing routed files into the working tree (`anneal materialize`):
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

pub use executor::{ActionResult, ExecError, Executor, LocalExecutor, PhaseTimings, SandboxError};
pub use fetch::FetchError;
pub use trust::compute_tier;
pub use verify::{
    prime_snapshot, verify_correctness_neutral, verify_warm_neutral, NeutralityReport,
};

// The stored trust vocabulary lives in `anneal-store` (it is what cache entries
// persist); re-exported here so downstream crates see one vocabulary.
pub use anneal_store::{CacheTier, EnforcementGrade, Provenance};
