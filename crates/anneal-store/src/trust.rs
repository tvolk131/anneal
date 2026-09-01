//! Trust plumbing (DESIGN.md §2.4, §2.8): enforcement grades, cache tiers,
//! and cache-entry provenance — the vocabulary that is **stored** with cache
//! entries, hence owned by the store crate. (`compute_tier`, which reads an
//! `Action`, stays in `anneal-exec`.)
//!
//! Three facts, kept carefully separate:
//!
//! - **What the action requested** — execution mode / cache policy, parts of the
//!   action contract; they key the action.
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
//! than from folklore.

use std::fmt;

/// What the platform's sandbox actually delivers for a **sealed** action.
/// Ordered: a floor check is `grade >= required`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EnforcementGrade {
    /// No isolation at all — the cfg fallback for platforms with no sandbox
    /// backend. Sealed semantics are *claimed but not delivered*; recorded so
    /// the claim can never silently pass for the real thing.
    Unenforced,
    /// Policy interception (macOS Seatbelt): undeclared access is denied and
    /// violations fail loudly, but the guarantee has known gaps — metadata
    /// visibility, the Darwin runtime allowlist, a deprecated mechanism.
    /// Action success does not *prove* input completeness.
    LoudBestEffort,
    /// Structural absence (Linux namespaces): undeclared inputs do not exist
    /// in the sandbox, so action success proves the declared input set
    /// complete.
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

    pub fn parse(s: &str) -> Option<Self> {
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

    pub fn parse(s: &str) -> Option<Self> {
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
/// cache entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// Producing host, `os-arch` (e.g. `macos-aarch64`). The *executing*
    /// platform — not the target configuration, which already keys the action.
    pub platform: String,
    pub grade: EnforcementGrade,
    pub tier: CacheTier,
}
