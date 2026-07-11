//! Trust plumbing (DESIGN.md §2.4, §2.8): enforcement grades, cache tiers,
//! and cache-entry provenance.
//!
//! Three facts, kept carefully separate:
//!
//! - **What the action requested** — [`ExecutionMode`] / [`CachePolicy`], parts
//!   of the action contract; they key the action.
//! - **What the platform delivered** — [`EnforcementGrade`], a fact about the
//!   host's sandbox machinery. It **never keys**: the same action built on a
//!   Mac and in CI is the same work; the grade governs where the *result* may
//!   be trusted, not what the result *is*.
//! - **Where the result may be trusted** — [`CacheTier`], computed from the two
//!   above. `Promotable` results are sound to share across machines; `Local`
//!   results are sound on the machine that produced them; `None` results are
//!   never cached at all.
//!
//! Every cache entry records its [`Provenance`] (producing platform, grade,
//! tier), so "why didn't this promote" is answerable from the store rather
//! than from folklore. The consumer/producer asymmetry (§2.8) is the eventual
//! payoff: when a shared cache lands, only `Promotable` entries are pushed —
//! a `LoudBestEffort` host consumes freely and produces nothing.

use std::fmt;

use crate::action::{Action, CachePolicy, ExecutionMode};

/// What the platform's sandbox actually delivers for a **sealed** action.
/// Ordered: a floor check is `grade >= required`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EnforcementGrade {
    /// No isolation at all — the cfg fallback for platforms with no sandbox
    /// backend. Sealed semantics are *claimed but not delivered*; recorded so
    /// the claim can never silently pass for the real thing. (DESIGN.md §2.8
    /// names two grades; the code carries this third because the no-backend
    /// fallback exists and "weaker than advertised" must include "absent".)
    Unenforced,
    /// Policy interception (macOS Seatbelt): undeclared access is denied and
    /// violations fail loudly, but the guarantee has known gaps — metadata
    /// visibility, the Darwin runtime allowlist, a deprecated mechanism.
    /// Action success does not *prove* input completeness.
    LoudBestEffort,
    /// Structural absence (Linux namespaces): undeclared inputs do not exist
    /// in the sandbox, so action success proves the declared input set
    /// complete. The pillar-one claim, kernel-enforced.
    Enforced,
}

impl EnforcementGrade {
    pub fn as_str(self) -> &'static str {
        match self {
            EnforcementGrade::Unenforced => "unenforced",
            EnforcementGrade::LoudBestEffort => "loud-best-effort",
            EnforcementGrade::Enforced => "enforced",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "unenforced" => Some(EnforcementGrade::Unenforced),
            "loud-best-effort" => Some(EnforcementGrade::LoudBestEffort),
            "enforced" => Some(EnforcementGrade::Enforced),
            _ => None,
        }
    }
}

impl fmt::Display for EnforcementGrade {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a result may be trusted. Computed, never declared (§2.4): an action
/// (or a rule author, later) can only *restrict* the computed tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CacheTier {
    /// Never cached (permeable/native modes, snapshot consumers).
    None,
    /// Sound on the producing machine only: the action mutates persistent
    /// tool state (snapshot owners), or it ran under enforcement too weak to
    /// prove its input set.
    Local,
    /// Sound to share across machines: deterministic, sealed, fully enforced —
    /// or pin-verified (fixed-output), where the digest check *is* the trust.
    Promotable,
}

impl CacheTier {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheTier::None => "none",
            CacheTier::Local => "local",
            CacheTier::Promotable => "promotable",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(CacheTier::None),
            "local" => Some(CacheTier::Local),
            "promotable" => Some(CacheTier::Promotable),
            _ => None,
        }
    }
}

impl fmt::Display for CacheTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What produced a cached result: the host platform, the enforcement grade it
/// delivered, and the tier computed at production time. Stored in every action
/// cache entry; surfaced on [`ActionResult`](crate::ActionResult).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// Producing host, `os-arch` (e.g. `macos-aarch64`). The *executing*
    /// platform — not the target configuration, which already keys the action.
    pub platform: String,
    pub grade: EnforcementGrade,
    pub tier: CacheTier,
}

/// The executing host platform, for provenance.
pub(crate) fn host_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The §2.4 tier table, over the existing action model (DESIGN.md Appendix A
/// ruling 4 maps the vocabularies: snapshot mutation ≈ `StateUse::Mutate`,
/// `Deterministic` ≈ `ByteDeterministic`):
///
/// ```text
/// mode != Sealed                          ⇒ None   (uncacheable by fiat)
/// NonCacheable | SnapshotConsuming        ⇒ None   (ditto)
/// FixedOutput                             ⇒ Promotable (pin-verified: the
///                                           framework checks the output digest
///                                           against the declared pin wherever
///                                           it lands, so the verification —
///                                           not the producing sandbox — is the
///                                           trust; grade-independent)
/// SnapshotBased (mutates tool state)      ⇒ at most Local
/// network permitted (non-FOD)             ⇒ at most Local (§2.4, no exceptions)
/// Deterministic ∧ Sealed ∧ Enforced       ⇒ Promotable
/// Deterministic ∧ Sealed ∧ grade < Enforced ⇒ Local  (§2.8)
/// ```
pub fn compute_tier(action: &Action, grade: EnforcementGrade) -> CacheTier {
    if !matches!(action.execution_mode, ExecutionMode::Sealed) {
        return CacheTier::None;
    }
    match action.cache_policy {
        CachePolicy::NonCacheable | CachePolicy::SnapshotConsuming => CacheTier::None,
        CachePolicy::FixedOutput { .. } => CacheTier::Promotable,
        CachePolicy::SnapshotBased => CacheTier::Local,
        CachePolicy::Deterministic => {
            if action.allows_network() {
                CacheTier::Local
            } else if grade == EnforcementGrade::Enforced {
                CacheTier::Promotable
            } else {
                CacheTier::Local
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, CachePolicy, ExecutionMode};
    use anneal_core::Digest;

    fn base() -> Action {
        Action::builder("a", vec!["./tool".to_owned()]).build()
    }

    #[test]
    fn deterministic_sealed_promotes_only_under_enforcement() {
        let action = base();
        assert_eq!(
            compute_tier(&action, EnforcementGrade::Enforced),
            CacheTier::Promotable
        );
        assert_eq!(
            compute_tier(&action, EnforcementGrade::LoudBestEffort),
            CacheTier::Local
        );
        assert_eq!(
            compute_tier(&action, EnforcementGrade::Unenforced),
            CacheTier::Local
        );
    }

    #[test]
    fn snapshot_mutation_caps_at_local_even_enforced() {
        let action = Action::builder("a", vec!["./tool".to_owned()])
            .snapshot(Digest::of(b"k"), vec!["target".into()])
            .build();
        assert_eq!(action.cache_policy, CachePolicy::SnapshotBased);
        assert_eq!(
            compute_tier(&action, EnforcementGrade::Enforced),
            CacheTier::Local
        );
    }

    #[test]
    fn network_caps_at_local_but_fixed_output_promotes() {
        let networked = Action::builder("a", vec!["./tool".to_owned()])
            .network(true)
            .build();
        assert_eq!(
            compute_tier(&networked, EnforcementGrade::Enforced),
            CacheTier::Local
        );
        let fod = Action::builder("a", vec!["./tool".to_owned()])
            .output("out", "blob")
            .network(true)
            .fixed_output(Digest::of(b"pin"))
            .build();
        assert_eq!(
            compute_tier(&fod, EnforcementGrade::LoudBestEffort),
            CacheTier::Promotable
        );
    }

    #[test]
    fn uncacheable_shapes_are_tier_none() {
        let mut consuming = Action::builder("a", vec!["./tool".to_owned()])
            .snapshot_restore(Digest::of(b"k"), vec!["target".into()])
            .build();
        assert_eq!(
            compute_tier(&consuming, EnforcementGrade::Enforced),
            CacheTier::None
        );
        consuming.execution_mode = ExecutionMode::Native;
        assert_eq!(
            compute_tier(&consuming, EnforcementGrade::Enforced),
            CacheTier::None
        );
        let permeable = {
            let mut a = base();
            a.execution_mode = ExecutionMode::Permeable;
            a
        };
        assert_eq!(
            compute_tier(&permeable, EnforcementGrade::Enforced),
            CacheTier::None
        );
    }

    #[test]
    fn grade_ordering_supports_floor_checks() {
        assert!(EnforcementGrade::Enforced > EnforcementGrade::LoudBestEffort);
        assert!(EnforcementGrade::LoudBestEffort > EnforcementGrade::Unenforced);
    }

    #[test]
    fn grade_and_tier_round_trip_their_wire_form() {
        for grade in [
            EnforcementGrade::Unenforced,
            EnforcementGrade::LoudBestEffort,
            EnforcementGrade::Enforced,
        ] {
            assert_eq!(EnforcementGrade::parse(grade.as_str()), Some(grade));
        }
        for tier in [CacheTier::None, CacheTier::Local, CacheTier::Promotable] {
            assert_eq!(CacheTier::parse(tier.as_str()), Some(tier));
        }
    }
}
