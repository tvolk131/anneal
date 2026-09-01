//! Debug crash injection for interruption testing
//! (`docs/proposals/anneal-store.md` §3.3).
//!
//! Production code calls [`crash_point`] at every labeled persistence phase.
//! It is a no-op unless the process was started with
//! `ANNEAL_CRASH_AFTER=<label>`, in which case the process aborts **at that
//! point** — a deterministic stand-in for `kill -9` / power loss that lets
//! table-driven tests assert, per phase: *the next run produces identical
//! declared outputs, whether it recovers warm or falls back to cold.*
//!
//! The label set is a test contract; the minimum phases live in the proposal:
//! `blob-put`, `action-insert`, `snapshot-manifest`, `snapshot-index`,
//! `warm-begin`, `warm-input-place`, `warm-commit`, `materialize-write`.
//!
//! `abort()` (not `exit()`) so the death is immediate and the surrounding test
//! can recognize it by signal rather than an exit code.

use std::sync::OnceLock;

/// Abort the process iff `ANNEAL_CRASH_AFTER` names this `label`.
pub fn crash_point(label: &str) {
    if enabled_label().as_deref() == Some(label) {
        eprintln!("anneal: crash injection at `{label}`");
        std::process::abort();
    }
}

/// The requested crash label, read once per process. Absent or empty disables
/// injection entirely.
fn enabled_label() -> Option<String> {
    static LABEL: OnceLock<Option<String>> = OnceLock::new();
    LABEL
        .get_or_init(|| {
            std::env::var("ANNEAL_CRASH_AFTER")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .clone()
}
