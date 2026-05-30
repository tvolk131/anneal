//! `anneal-analysis` — turns a [`TargetGraph`] into an action graph (§3.1, §3.3).
//!
//! Analysis is the middle phase: `loading → analysis → execution`. It visits a
//! requested target and its transitive dependencies in **dependency order**, calls
//! each rule's [`analyze`] with a [`RuleContext`] whose `deps` carry the
//! **already-computed providers** of that target's dependencies, **memoizes** each
//! configured target so a diamond is analyzed once, and collects every emitted
//! [`Action`] into an ordered [`ActionGraph`] for the executor.
//!
//! [`TargetGraph`]: anneal_loader::TargetGraph
//! [`analyze`]: anneal_rules::Rule::analyze
//! [`RuleContext`]: anneal_rules::RuleContext
//! [`Action`]: anneal_exec::Action
//!
//! # Milestone 1 scope
//!
//! * **One configuration per analysis.** The formal unit is the configured target
//!   `(label, configuration)`; with a single configuration and no transitions
//!   (§6.4) yet, memoization is keyed by label. The key gains the configuration when
//!   transitions arrive.
//! * **Single `TargetGraph`.** Every dependency must be present in the graph handed
//!   to the [`Analyzer`]; cross-package on-demand loading is a workspace-loader
//!   concern, kept out of analysis.
//! * **Resolved providers only.** `genrule → {filegroup, alias}` threads concrete
//!   content digests. Consuming a producer's *unresolved* outputs (`genrule →
//!   genrule`) needs the action graph to carry inter-action edges resolved at
//!   execution time — a kernel extension that is the next increment.

mod analyzer;
mod error;

pub use analyzer::{ActionGraph, AnalyzedTarget, Analyzer};
pub use error::AnalysisError;
