# 1. Snapshot keying and the rule↔engine trust boundary

- **Status:** Accepted — 2026-06-22
- **Contract it governs:** `docs/rules.md` §5–§6 (the trust boundary and how the snapshot key
  is formed). This note records the *decision and its rationale*; `rules.md` is the durable
  contract. Background invariants: `build-system-design.md` §1.4 (correctness-neutral),
  §8.2 (snapshot protocol), §3.2 ("wrap, don't decompose"), §2.4/§2.8 (cache tiers / trust).

## Context

The snapshot key (which warm `target/`-style tree is safe to restore) is formed today by each
rule hand-building a shard (`cargo_workspace.rs::target_state_shard`). Reviewing that surfaced
two things:

1. **A footgun in the allowlist shape.** A hand-built "these things key the snapshot" list is
   forgettable: the action env was in the *action* key but had been omitted from the snapshot
   shard. (No live miscache resulted — `cargo_workspace`'s env is closure-derived, axis-derived,
   or constant, all already covered — but the omission was easy and silent.)
2. **A tempting generalization:** give rules a *declarative* way to mark which inputs the wrapped
   tool absorbs incrementally, so the engine excludes them from the snapshot key and keys on the
   rest. This would let an author recover warm-reuse performance across, e.g., a lockfile bump by
   trusting the tool's per-unit fingerprinting.

The decision is whether to add that declarative incremental-exclusion capability ("**Later**") or
to keep the engine deriving the snapshot key conservatively with no such verb ("**Now**"), and how
to reconcile that with the irreducible fact that *some* exclusion (the source tree) must exist or
the snapshot is never reused.

## Decision

1. **No incremental-safe exclusion verb, ever.** A rule cannot tell the engine that an input or
   env is "absorbed incrementally" and may be left out of the snapshot key. Rules shape the key
   only by shard contents, and shard choices can only make the key *finer* (more conservative) —
   under-keying a tool-relevant input stays inexpressible.
2. **The one admitted incrementality judgment is coarse and channel-encoded.** "This directory is
   the wrapped tool's incremental domain" is carried by the declaration channel
   (`source_tree(".")` → excluded from the snapshot key; explicit `input()` / lockfile / `data` →
   keyed), not by a per-input flag. Conservative-by-default falls out: anything not in the
   tool-domain channel is keyed.
3. **Containment is part of the design, not an afterthought.** Snapshot owners are capped at the
   `Local` cache tier, so warm-derived outputs never reach the shared cache (populated only by the
   cold Hermetic arm, content-addressed on full identity). The attestation epoch (folded into the
   state key) makes the residual trust globally revocable.
4. **Follow-ups that keep "Now" both safe and fast** (tracked in `TODO.md`, not all shipped):
   fold author-added env into the snapshot key (or forbid it on snapshot owners) to close the
   env-vs-shard residual; derive the lockfile shard component from the *resolved dependency set*
   so cosmetic `Cargo.lock` churn doesn't force a cold rebuild; broaden the cold-vs-warm verifier
   to real targets and add env-varying / non-cargo fixtures.

## Rationale

- **The asymmetry is lexical, not a tradeoff.** Over-keying the snapshot costs only a cold rebuild
  (the snapshot key is excluded from action identity, so it can only flip the warm/cold branch —
  never produce a wrong cache entry). Under-keying lets two configurations share one warm tree as
  live state, silently and deferred. When one option's worst case is "slower" and the other's is
  "silently wrong, later," correctness is simply prior; there is no exchange rate to compute.
- **The flexibility has no legitimate non-anti-pattern use.** Every input an author might mark
  incremental-safe is (a) the source tree — already excluded via the channel; (b) output-relevant
  — excluding it *is* the §1.4 cardinal sin; or (c) output-irrelevant churn — already poisoning the
  *action* cache, whose correct fix is to stop feeding it to the action, fixing both caches. So the
  verb's entire residual value is to legitimize bugs. (Note: "rule changes are rare" is *not* the
  argument — it bounds frequency, not blast radius. The argument is no-legitimate-use.)
- **It matches the grain the design already chose.** Cacheability and cache tier are
  derived-and-enforced, never claimed; a consumer's levers move only *toward* non-cacheable; tier
  may be restricted, never escalated (`rules.md` §4; DESIGN §2.4). An incremental-safe exclusion is
  a finer, optimization-direction claim — the one thing refused everywhere else. It would also be a
  *cost-free* trust with no revocation lever: exactly the "unmarked attestation" shape §2.4 already
  identified and deleted.
- **Guaranteed neutrality is the pure-vs-stateful line.** A structural guarantee exists for
  content-addressed accelerators (the action cache) and is impossible for a stateful tool-owned
  accelerator (the snapshot), whose neutrality is a counterfactual about the cold build the engine
  can't observe. That impossibility is the price of "wrap, don't decompose" — earning the guarantee
  would mean decomposing into per-unit content-addressed actions (the Bazel model anneal rejects).
  So the snapshot trust is minimized, contained, and revocable — not eliminated.

## Alternatives considered and rejected

- **"Later" — a declarative incremental-exclusion capability.** Rejected: a cost-free,
  engine-unverifiable trust claim with no revocation lever and no legitimate use the engine can't
  already obtain safely. It would re-open the deleted §2.4 unmarked-attestation wound and add a
  *second* unenforced-judgment surface (free-form exclusion) on top of the one that's irreducible
  (shard population, which can only err by being too coarse).
- **Full decomposition (per-crate content-addressed actions, the Bazel/`rules_rust` model).**
  Would yield a structural neutrality guarantee, but by abandoning "wrap, don't decompose" (§3.2) —
  i.e. by re-implementing the incrementality every wrapped tool already implements. This is the
  thing anneal exists *not* to do.
- **Lean entirely on the verifier (trust-but-verify as the primary mechanism).** Rejected as
  *primary*: the cold-vs-warm harness has one-sided error (agreement is evidence, not proof) and is
  structurally blind to env/config-driven staleness unless it varies them across the pair. It is a
  tripwire and a release gate, not an invariant upholder — valuable as a backstop, not a license.

## Consequences

- A lockfile / toolchain / flag / (eventually) env change abandons the warm `target/` and rebuilds
  cold. This is correct and conservative; the perf cost is recoverable and self-healing. GAP-1
  (resolved-dep-set lockfile shard) recovers the biggest instance (cosmetic churn) without making
  under-keying expressible.
- An author who is *genuinely, correctly* certain an input is incrementally absorbed cannot encode
  that speedup — they pay a cold rebuild on its change. This is deliberate: it is the footgun being
  removed, paid for by everyone who would otherwise be wrong.
- **"Now" does not make wrong builds structurally impossible** — it minimizes the unenforced-judgment
  surface to one (shard population) and quarantines the irreducible trust (snapshot neutrality) to a
  single machine, recoverable and revocable, while the *shared* cache — the correctness that actually
  matters — stands entirely on the cold, content-addressed Hermetic arm and depends on snapshot
  neutrality not at all.
