//! Typed persistent state (DESIGN.md §2.1, §2.5, §2.6 — Appendix A ruling 4):
//! the state taxonomy grown **in place** out of the snapshot mechanics.
//!
//! The vocabulary bridge, made code: a [`StateKind::PhaseSeparated`] tree is
//! produced by one action and consumed read-only (pnpm's node_modules — today's
//! *shared* snapshot); a [`StateKind::Interleaved`] tree is mutated by the very
//! actions that read it (cargo's `target/` — today's *private* snapshot), and
//! declaring it **is** the rule author vouching for the wrapped tool's internal
//! invalidation logic, so it demands an [`Attestation`].
//!
//! What the types enforce, at analysis time:
//! - `Mutate` exists only for `Interleaved`; `Produce`/`Read` only for
//!   `PhaseSeparated`. **`Read` of interleaved state does not exist** (§2.5):
//!   its contents are in no key, so a reader would be silently stale-able.
//! - A [`StateHandle`] is mintable only via `RuleContext::declare_state`, which
//!   scopes the key by the declaring rule's kind — cross-rule sharing (and
//!   mutating under another rule's attestation) is inexpressible (§2.6).
//! - The attestation **epoch folds into the state key**, so bumping it mass-
//!   invalidates every warm tree and cache entry derived under the old epoch —
//!   revocation as one constant change, not `anneal clean` folklore.
//!
//! What is deliberately *not* yet enforced, recorded so it isn't mistaken for
//! enforced: single-producer for phase-separated state (today each declaring
//! target registers an identical producer action and they dedup by action key);
//! one state per action (the underlying action model carries one snapshot —
//! multi-state actions arrive with the exec-layer state work); and the
//! produce-vs-mutate tier distinction (the trust layer currently treats every
//! snapshot owner as Local-capped, which is conservative for producers).

use std::path::PathBuf;

use anneal_core::Digest;
use anneal_exec::ActionBuilder;

use crate::rule::RuleError;

/// The signed-in-code acknowledgment accompanying interleaved state (§2.6).
/// `epoch` must be a per-rule-version constant, never computed from attrs:
/// bumping it is the revocation mechanism for a discovered soundness bug in
/// the wrapped tool or the rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attestation {
    pub epoch: u32,
    /// Human-readable scope of what is being vouched for; surfaced by
    /// `anneal query --explain-trust` when that lands.
    pub rationale: &'static str,
}

/// Whether the wrapped tool's state tree tolerates concurrent mutators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concurrency {
    /// At most one mutating action per state key at a time. (This is what the
    /// per-key warm locks already provide; `Exclusive` names it.)
    Exclusive,
    /// The tool's state is safe under concurrent mutators (content-addressed
    /// GOCACHE). Covered by the attestation. The current scheduler still
    /// serializes per key — over-locking is slower, never unsound.
    SharedSafe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateKind {
    /// One producer, read-only consumers. No trust delegated — the framework
    /// enforces the invariants mechanically — so no attestation exists.
    PhaseSeparated,
    /// Mutated by its readers. Declaring it is the vouching: attestation
    /// required, and every mutating action is capped at the Local cache tier
    /// by the trust layer (snapshot owners never promote).
    Interleaved {
        concurrency: Concurrency,
        attestation: Attestation,
    },
}

/// A persistent state tree a rule's actions may use: identity (namespace +
/// shard), kind, and the working-directory paths the tree occupies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistentStateDecl {
    /// e.g. `"cargo-target"`. Implicitly scoped by the declaring rule's kind.
    pub namespace: &'static str,
    /// Sharding values (toolchain identity, lockfile digest, target triple,
    /// axis values…) — each shard gets its own tree, so exclusivity costs
    /// parallelism only within a shard.
    pub shard: Vec<String>,
    pub kind: StateKind,
    /// Paths (relative to the action working directory) the tree occupies,
    /// e.g. `["target"]` or `["node_modules", ".anneal-pnpm-store"]`.
    pub paths: Vec<PathBuf>,
}

/// Handle to a declared state tree. Mintable only via
/// `RuleContext::declare_state` — private fields, no other constructor — so a
/// `mutate_state` grant cannot exist without an attestation having been
/// written. The compiler checks the loudest invariant in the design (§3.3).
#[derive(Debug, Clone)]
pub struct StateHandle {
    pub(crate) key: Digest,
    pub(crate) kind: StateKind,
    pub(crate) namespace: &'static str,
    pub(crate) paths: Vec<PathBuf>,
}

impl StateHandle {
    /// The state key: rule-scoped, shard-qualified, epoch-versioned. This is
    /// the snapshot key of every action using the state.
    pub fn key(&self) -> Digest {
        self.key
    }

    pub fn namespace(&self) -> &'static str {
        self.namespace
    }
}

/// Derive the state key. Folds in the declaring rule's kind (§2.6 scoping),
/// the namespace and shard (identity), the kind discriminant, and — for
/// interleaved state — the attestation epoch (revocation).
pub(crate) fn state_key(rule_kind: &str, decl: &PersistentStateDecl) -> Digest {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"anneal-state-v1\n");
    buf.extend_from_slice(rule_kind.as_bytes());
    buf.push(0);
    buf.extend_from_slice(decl.namespace.as_bytes());
    buf.push(0);
    for shard in &decl.shard {
        buf.extend_from_slice(shard.as_bytes());
        buf.push(0);
    }
    match &decl.kind {
        StateKind::PhaseSeparated => buf.extend_from_slice(b"phase-separated"),
        StateKind::Interleaved { attestation, .. } => {
            buf.extend_from_slice(b"interleaved\n");
            buf.extend_from_slice(&attestation.epoch.to_le_bytes());
        }
    }
    Digest::of(&buf)
}

/// The `StateUse` grants (§2.1), as builder extensions lowering to the snapshot
/// mechanics. Each grant checks kind legality at analysis time and rejects a
/// second state on one action (the action model carries one snapshot today).
pub trait StateActionExt: Sized {
    /// Sole producer of a phase-separated tree: owns and saves the snapshot.
    fn produce_state(self, state: &StateHandle) -> Result<Self, RuleError>;
    /// Read-only consumer of a phase-separated tree: restores, never saves,
    /// never caches as if it owned the tree.
    fn read_state(self, state: &StateHandle) -> Result<Self, RuleError>;
    /// Read-write use of an interleaved tree — legal only because the handle's
    /// existence proves an attestation was written. Lowered to a *private*
    /// snapshot: the warm tree is the live copy, capped Local by the trust layer.
    fn mutate_state(self, state: &StateHandle) -> Result<Self, RuleError>;

    /// `mutate_state` when a handle is present; pass-through otherwise. The
    /// Hermetic-arm convenience (DESIGN.md §4.4): rules declare interleaved
    /// state only under `ExecMode::Incremental` and thread the `Option` —
    /// hermetic actions get no grant, and action validation enforces that no
    /// mutator slips through regardless.
    fn mutate_state_opt(self, state: Option<&StateHandle>) -> Result<Self, RuleError> {
        match state {
            Some(state) => self.mutate_state(state),
            None => Ok(self),
        }
    }
}

impl StateActionExt for ActionBuilder {
    fn produce_state(self, state: &StateHandle) -> Result<Self, RuleError> {
        guard_single_state(&self)?;
        match state.kind {
            StateKind::PhaseSeparated => Ok(self.snapshot(state.key, state.paths.clone())),
            StateKind::Interleaved { .. } => Err(RuleError::Message(format!(
                "state {:?}: interleaved state has no producer — every user is a \
                 mutator; use mutate_state",
                state.namespace
            ))),
        }
    }

    fn read_state(self, state: &StateHandle) -> Result<Self, RuleError> {
        guard_single_state(&self)?;
        match state.kind {
            StateKind::PhaseSeparated => Ok(self.snapshot_restore(state.key, state.paths.clone())),
            // §2.5: a reader of interleaved state has an input that exists in
            // no key — silently stale-able, so the arm does not exist.
            StateKind::Interleaved { .. } => Err(RuleError::Message(format!(
                "state {:?}: Read of interleaved state is forbidden (DESIGN.md \
                 §2.5) — its contents are content-tracked in no key, so a reader \
                 would be silently stale-able; extract outputs from the mutating \
                 action instead",
                state.namespace
            ))),
        }
    }

    fn mutate_state(self, state: &StateHandle) -> Result<Self, RuleError> {
        guard_single_state(&self)?;
        match state.kind {
            StateKind::Interleaved { .. } => {
                Ok(self.snapshot_private(state.key, state.paths.clone()))
            }
            StateKind::PhaseSeparated => Err(RuleError::Message(format!(
                "state {:?}: phase-separated state is immutable for consumers — \
                 produce_state for the one producer, read_state for everyone else",
                state.namespace
            ))),
        }
    }
}

fn guard_single_state(builder: &ActionBuilder) -> Result<(), RuleError> {
    if builder.snapshot_is_set() {
        return Err(RuleError::Message(
            "action already uses a persistent state tree; one state per action \
             today (multi-state actions arrive with the exec-layer state work)"
                .to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anneal_exec::Action;

    fn handle(kind: StateKind) -> StateHandle {
        let decl = PersistentStateDecl {
            namespace: "test-state",
            shard: vec!["shard-a".into()],
            kind: kind.clone(),
            paths: vec![PathBuf::from("state")],
        };
        StateHandle {
            key: state_key("test_rule", &decl),
            kind,
            namespace: decl.namespace,
            paths: decl.paths,
        }
    }

    fn interleaved(epoch: u32) -> StateKind {
        StateKind::Interleaved {
            concurrency: Concurrency::Exclusive,
            attestation: Attestation {
                epoch,
                rationale: "test",
            },
        }
    }

    fn builder() -> ActionBuilder {
        Action::builder("a", vec!["./tool".to_owned()])
    }

    #[test]
    fn use_legality_matrix() {
        let phase = handle(StateKind::PhaseSeparated);
        let inter = handle(interleaved(1));

        assert!(builder().produce_state(&phase).is_ok());
        assert!(builder().read_state(&phase).is_ok());
        assert!(builder().mutate_state(&phase).is_err());

        assert!(builder().produce_state(&inter).is_err());
        assert!(
            builder().read_state(&inter).is_err(),
            "§2.5: no Read of interleaved"
        );
        assert!(builder().mutate_state(&inter).is_ok());
    }

    #[test]
    fn second_state_on_one_action_is_rejected() {
        let phase = handle(StateKind::PhaseSeparated);
        let b = builder().read_state(&phase).unwrap();
        assert!(b.read_state(&phase).is_err());
    }

    #[test]
    fn epoch_bumps_the_key_and_rule_kind_scopes_it() {
        let decl = |kind: StateKind| PersistentStateDecl {
            namespace: "ns",
            shard: vec!["s".into()],
            kind,
            paths: vec![],
        };
        let e1 = state_key("rule_a", &decl(interleaved(1)));
        let e2 = state_key("rule_a", &decl(interleaved(2)));
        assert_ne!(e1, e2, "epoch bump revokes: every derived key changes");

        let other_rule = state_key("rule_b", &decl(interleaved(1)));
        assert_ne!(
            e1, other_rule,
            "rule-kind scoping: same decl, different rule, different key"
        );

        let phase_a = state_key("rule_a", &decl(StateKind::PhaseSeparated));
        assert_ne!(e1, phase_a, "kind is part of identity");
    }

    #[test]
    fn mutating_action_lowers_to_private_snapshot() {
        let inter = handle(interleaved(1));
        let action = builder().mutate_state(&inter).unwrap().build();
        assert_eq!(action.snapshot_key(), Some(inter.key()));
    }
}
