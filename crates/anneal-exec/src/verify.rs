//! The correctness-neutral verification harness (§1.4, §22).
//!
//! The central invariant: **restoring a snapshot may make a build faster but must
//! never change its semantic output.** This harness is the release-blocker gate that
//! checks it. Given a snapshot-based action whose snapshot was populated by an
//! earlier build, it runs the action two ways and compares the declared outputs:
//!
//! 1. **cold** — a fresh sandbox with no snapshot restored (a clean build);
//! 2. **warm** — the same sandbox with the snapshot restored (an incremental build
//!    on top of the prior state).
//!
//! Both run at the **same** sandbox path, so any output difference is attributable to
//! the snapshot — exactly the thing the invariant forbids. Any divergence is a
//! release blocker, not a warning (§22).

use std::collections::BTreeMap;

use anneal_core::Digest;

use crate::action::Action;
use crate::executor::{ActionResult, ExecError, LocalExecutor};

/// The shared sandbox the verifier (and snapshot priming) run in. Priming and both
/// verification runs use the **same** path so a snapshot's embedded paths match those
/// a fresh build at this path would produce — otherwise reused (unchanged) artifacts
/// would differ from cold ones for path reasons alone.
const VERIFY_SANDBOX: &str = "verify-neutral";

/// The outcome of a correctness-neutrality check: the cold and warm output digests.
#[derive(Debug, Clone)]
pub struct NeutralityReport {
    pub cold: BTreeMap<String, Digest>,
    pub warm: BTreeMap<String, Digest>,
}

impl NeutralityReport {
    /// True iff the snapshot-restored build produced byte-identical outputs to the
    /// cold build.
    pub fn is_neutral(&self) -> bool {
        self.cold == self.warm
    }

    /// Output names whose cold and warm digests differ (or that appear in only one).
    /// Empty iff [`is_neutral`](Self::is_neutral).
    pub fn divergences(&self) -> Vec<String> {
        let mut names: Vec<String> = self.cold.keys().chain(self.warm.keys()).cloned().collect();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .filter(|name| self.cold.get(name) != self.warm.get(name))
            .collect()
    }
}

/// Run `action` cold and snapshot-warm in an identical sandbox and compare outputs.
///
/// A snapshot for `action`'s key must already exist (populated by an earlier build);
/// otherwise the warm run is also a cold start and the check is vacuously neutral.
/// Neither run reads or writes the action cache, and neither saves a new snapshot.
pub fn verify_correctness_neutral(
    exec: &LocalExecutor,
    action: &Action,
) -> Result<NeutralityReport, ExecError> {
    // Same sandbox path for both runs ⇒ embedded paths match ⇒ outputs comparable.
    let cold = exec.run_uncached(
        action,
        VERIFY_SANDBOX,
        /*restore*/ false,
        /*save*/ false,
    )?;
    let warm = exec.run_uncached(
        action,
        VERIFY_SANDBOX,
        /*restore*/ true,
        /*save*/ false,
    )?;
    Ok(NeutralityReport {
        cold: cold.outputs,
        warm: warm.outputs,
    })
}

/// Build `action` and **save** its snapshot, in the verifier's sandbox. Call this on
/// an earlier source state to populate the snapshot that
/// [`verify_correctness_neutral`] will restore for the warm run.
pub fn prime_snapshot(exec: &LocalExecutor, action: &Action) -> Result<ActionResult, ExecError> {
    exec.run_uncached(
        action,
        VERIFY_SANDBOX,
        /*restore*/ false,
        /*save*/ true,
    )
}

/// Verify that **warm-sandbox reuse** is correctness-neutral (§5, §1.4) — distinct from
/// [`verify_correctness_neutral`], which checks the CAS-restore path. Here the warm path
/// (keep `target/` in place + in-place sync of only the changed inputs) is the mechanism
/// under test.
///
/// `baseline` and `edited` are the same action at two source states (so they share a
/// snapshot key — e.g. a cargo build action before/after an edit). The check:
///
/// 1. cold-populate the warm dir at `baseline`;
/// 2. **warm** — reuse it and sync `baseline → edited` in place (the incremental build);
/// 3. **cold** — wipe the same warm dir and clean-build `edited`.
///
/// Both (2) and (3) build `edited` at the *same* per-key warm path, so any output
/// difference is attributable to the sync — exactly the mtime-hazard backstop. Neither
/// touches the action cache. Any divergence is a release blocker (§22).
pub fn verify_warm_neutral(
    exec: &LocalExecutor,
    baseline: &Action,
    edited: &Action,
) -> Result<NeutralityReport, ExecError> {
    exec.run_warm_uncached(baseline, /*fresh*/ true)?; // populate the warm dir at baseline
    let warm = exec.run_warm_uncached(edited, /*fresh*/ false)?; // reuse + sync baseline→edited
    let cold = exec.run_warm_uncached(edited, /*fresh*/ true)?; // same path, clean build of edited
    Ok(NeutralityReport {
        cold: cold.outputs,
        warm: warm.outputs,
    })
}
