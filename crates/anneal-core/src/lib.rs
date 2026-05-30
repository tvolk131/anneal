//! `anneal-core` — the shared vocabulary of the build system.
//!
//! These are the types that appear in every cache key and cross every module
//! boundary: content [`Digest`]s, target [`Label`]s, and the [`Configuration`]
//! (a [`Platform`] plus the five universal axis values, §6.2).
//!
//! # Why this crate is deliberately thin
//!
//! `MILESTONE-1-PLAN.md` flags this as the one shallow-module risk: a "vocabulary"
//! crate easily degenerates into a junk-drawer of public-field structs. The
//! discipline here is the antidote:
//!
//! * Types with **invariants** ([`Digest`], [`Label`], [`Platform`]) keep their
//!   fields **private** and are built only through validating constructors.
//! * Types that are **pure records with no cross-field invariant** ([`AxisValues`])
//!   may expose fields — that leaks no representation because there is none to hide.
//! * **Behavior** belongs to the deeper module that owns the concept. In particular
//!   cache-key *hashing* lives in `anneal-exec`, not here; this crate only provides
//!   the stable, canonical *data* to hash (e.g. [`AxisValues::consumed`]).

mod config;
mod digest;
mod label;

pub use config::{
    Axis, AxisValues, Configuration, Coverage, DebugInfo, Lto, OptLevel, Platform, Sanitizer,
    ALL_AXES,
};
pub use digest::{Digest, DigestParseError};
pub use label::{Label, LabelParseError};
