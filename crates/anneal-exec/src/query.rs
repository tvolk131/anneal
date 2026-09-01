//! Query *execution* — the engine half of the query contract. The spec
//! itself ([`QuerySpec`], its builder, and the [`QueryRunner`] capability)
//! lives in `anneal-action`; this module owns what only the engine can know:
//! the cache-key namespace and the stable sandbox root.
//!
//! ## Sandbox-root stability
//!
//! Tools embed absolute paths in their stdout (`cargo metadata` reports
//! `workspace_root`, `manifest_path`, `target_directory`). Byte-determinism
//! therefore requires the sandbox path a query runs at to be **stable across
//! runs whose output should converge** — in particular across *input edits
//! that leave the output identical*, which is exactly the early-cutoff case.
//! So [`query_identity`] is derived from the query's identity (command, env,
//! toolchains, working dir) and deliberately **not** from its input digests (a
//! per-key root would bake the input digest into the emitted paths and kill
//! cutoff on every edit).
//!
//! Platform asymmetry, recorded: on Linux the sandbox binds the root at a
//! fixed guest path (`/work`), so emitted paths are machine-independent and
//! query outputs can converge across machines. On macOS there is no mount
//! namespace; the *host* path leaks into the output, so query outputs are
//! stable per-checkout but not across machines — consistent with §2.8, where
//! `LoudBestEffort` hosts never produce into shared caches anyway.

use anneal_action::Action;
use anneal_core::Digest;

/// The logical output name under which captured stdout is stored in the action
/// cache. Namespaced so a query entry can never be confused with a declared
/// file output.
pub(crate) const QUERY_STDOUT: &str = "__query_stdout";

/// The cache key for a query: the full action digest (inputs included),
/// namespaced so query entries and ordinary action entries can never collide.
pub(crate) fn query_key(action: &Action) -> Digest {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"anneal-query-v1\n");
    buf.extend_from_slice(anneal_action::action_digest(action).as_bytes());
    Digest::of(&buf)
}

/// The query's *identity*: everything that defines which query this is —
/// command, env, toolchain identities, working directory — and **nothing that
/// changes when its inputs change**. This keys the stable sandbox root (see
/// the module docs for why input digests must stay out of it). Two runs of
/// the same query over edited inputs land at the same path, so tool-emitted
/// absolute paths are byte-stable and early cutoff survives.
pub(crate) fn query_identity(action: &Action) -> Digest {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"anneal-query-identity-v1\n");
    for arg in action.command() {
        buf.extend_from_slice(arg.as_bytes());
        buf.push(0);
    }
    for (k, v) in action.env() {
        buf.extend_from_slice(k.as_bytes());
        buf.push(0);
        buf.extend_from_slice(v.as_bytes());
        buf.push(0);
    }
    for tc in action.toolchains().values() {
        buf.extend_from_slice(tc.identity().as_bytes());
        buf.push(0);
    }
    buf.extend_from_slice(action.working_directory().as_os_str().as_encoded_bytes());
    Digest::of(&buf)
}
