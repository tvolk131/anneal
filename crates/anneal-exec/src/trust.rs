//! Trust computation (DESIGN.md §2.4, §2.8). The *stored* vocabulary —
//! [`EnforcementGrade`], [`CacheTier`], [`Provenance`] — lives in
//! `anneal-store` (it is what cache entries persist); this module computes a
//! tier from an [`Action`] and its delivered grade, and names the host.

pub use anneal_store::{CacheTier, EnforcementGrade};

use anneal_action::{Action, CachePolicy, ExecutionMode};

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
    if !matches!(action.execution_mode(), ExecutionMode::Sealed) {
        return CacheTier::None;
    }
    match action.cache_policy() {
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
        assert_eq!(action.cache_policy(), CachePolicy::SnapshotBased);
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
        let consuming = Action::builder("a", vec!["./tool".to_owned()])
            .snapshot_restore(Digest::of(b"k"), vec!["target".into()])
            .build();
        assert_eq!(
            compute_tier(&consuming, EnforcementGrade::Enforced),
            CacheTier::None
        );
        let native = Action::builder("a", vec!["./tool".to_owned()])
            .snapshot_restore(Digest::of(b"k"), vec!["target".into()])
            .mode(ExecutionMode::Native)
            .build();
        assert_eq!(
            compute_tier(&native, EnforcementGrade::Enforced),
            CacheTier::None
        );
        let permeable = Action::builder("a", vec!["./tool".to_owned()])
            .mode(ExecutionMode::Permeable)
            .build();
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
