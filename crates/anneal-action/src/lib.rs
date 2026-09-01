//! `anneal-action` — the rule/engine contract.
//!
//! Everything a rule *declares* and everything the engine *keys on*, with no
//! execution attached: the [`Action`] model (built only through
//! [`Action::builder`]), its cache identity ([`action_digest`] — a pure
//! function of the model, pinned by variation tests and a golden digest), and
//! the tool-query contract ([`QuerySpec`] plus the [`QueryRunner`] capability
//! the engine implements).
//!
//! This crate is deliberately dependency-light (`anneal-core` only): rules
//! depend on the contract without hauling in the execution engine, and the
//! engine depends on the contract without owning it. The [`Action`] fields are
//! `pub(crate)` — readable by the identity fold, constructible only through
//! the builder — and execution reads an action through its declared accessor
//! surface, not its representation.
//!
//! What deliberately lives *elsewhere*: running actions (`anneal-exec`:
//! scheduler, sandbox, warm reuse), persisting results (`anneal-store`), and
//! computing trust tiers (`anneal-exec::compute_tier`, which reads an action
//! but is engine policy).

mod action;
mod identity;
mod query;

pub use action::{
    Action, ActionBuilder, ActionError, CachePolicy, ExecutionMode, Input, InputSource, Toolchain,
};
pub use identity::{action_digest, EXECUTION_CONTRACT_VERSION};
pub use query::{QueryBuilder, QueryError, QueryResult, QueryRunner, QuerySpec};
