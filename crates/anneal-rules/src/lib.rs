//! `anneal-rules` — the [`Rule`] trait and the first-party rules.
//!
//! This crate *is* the system/rule boundary (§5.3): "system provides policy; rules
//! provide mechanism." The narrow interface is [`Rule::analyze`], which maps a
//! target's attributes, configuration, and dependency providers to a set of
//! [`anneal_exec::Action`]s plus the typed [`ProviderSet`] it exposes to dependents
//! (§5.2).
//!
//! The system side hands the rule a [`RuleContext`]: typed attribute access,
//! the [`Configuration`], the providers of already-analyzed dependencies, and a
//! source-file resolver that reads files into the CAS. The rule decides *what*
//! command to run; the system (the kernel in `anneal-exec`) decides *how* it runs.
//!
//! [`Configuration`]: anneal_core::Configuration
//!
//! # Milestone 1 scope
//!
//! Three rules: [`rules::FileGroup`], [`rules::Alias`], [`rules::GenRule`]. `genrule`
//! consumes **resolved** source files (paths on disk, and `filegroup` providers) and
//! emits one [`anneal_exec::Action`]. Consuming the *produced* outputs of another
//! action as inputs (the genrule→genrule action graph) needs post-execution digest
//! threading and arrives with `anneal-analysis`. Per-rule axis interpretation (§13.6)
//! is also deferred — `genrule` is configuration-invariant for now.

mod attrs;
mod context;
mod providers;
mod rule;
mod rules;
mod schema;

pub use attrs::{AttrError, AttrValue, Attrs};
pub use context::{ResolvedDep, RuleContext};
pub use providers::{Artifact, ArtifactSource, FileSet, ProviderSet};
pub use rule::{Analysis, Rule, RuleError};
pub use rules::{builtin_rules, Alias, FileGroup, GenRule, RuleRegistry};
pub use schema::{AttrSchema, AttrType};
