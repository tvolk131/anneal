# What a rule is

> Companion to `build-system-design.md` (§5 *Rules and the Rule Model*, §13 *First-Party
> Rules*, §14.6 *the phase wall*). The main doc says a rule "produces actions"; this note
> develops what a rule *is* from first principles — its mechanical contract, the eight
> obligations that contract carries, the **inference↔declaration spectrum** that decides
> how much a rule can automate, and the line between hermeticity and determinism that
> governs what a rule may safely cache. Written after the `pnpm_workspace` design pushed
> on every one of these edges.

## 1. The mechanical truth

Strip away intent and the `Rule` trait says exactly what a rule is:

```
analyze(ctx) -> Analysis { actions, providers }
```

**A rule is a pure function from a configured-target context to a slice of the action
graph plus a set of providers.** That is the entire contract. Everything else in this
document is an *obligation* or a *freedom* of that function — not a separate mechanism.

Two consequences fall directly out of "pure function, run in the analysis phase":

- It **cannot execute anything.** It emits actions to be run later; it does not run them.
- It **cannot see generated content.** It runs in analysis, before execution, so any
  artifact a downstream action will produce does not exist yet. A rule reads *source* and
  *static declared structure* only. (This is the §14.6 phase wall — the reason a generated
  `Cargo.toml` / `pnpm-workspace.yaml` is impossible to consume as an edge, but a generated
  `config.json` is fine.)

## 2. The eight obligations

The function signature is small; the obligations it carries are not. A rule:

1. **Wraps an opaque inner engine** (§3.2, "wrap, don't decompose"). Cargo, pnpm, Nickel
   stay the source of truth for their own world; the rule emits *coarse* actions and lets
   the engine own the inner loop. The rule does not model rustc invocations or tsc passes.

2. **Produces providers, not just actions.** `Analysis` is `{actions, providers}`, and the
   providers are the under-rated half. Actions are the *work*; providers are the
   **interface the rule offers upward** to anything that depends on it (providers flow up,
   configuration flows down — §5.4). Routing a generated artifact across a language
   boundary is entirely a provider/consumer story; no action "knows" about the consumer.

3. **Is a pure analysis-phase function** — see §1. The phase wall is not a limitation to
   work around; it is what *distinguishes a rule from a build step*. A rule decides the
   shape of the graph; it does not participate in running it.

4. **Claims an ownership territory.** A workspace rule stakes an *exclusive* claim over a
   package subtree — `owner(path)` is the nearest enclosing package (§1.5). A rule is not
   just behavior; it is a territorial boundary, and that boundary is what makes `affected`
   and `why` sound. (Hence "one workspace rule per package directory.")

5. **Translates the universal configuration axes into the engine's native knobs, and
   declares which axes it consumes.** `opt_level → --release` for Cargo; `opt_level →
   minification` for pnpm; nothing for Nickel (§13.6). The *consumed-axes declaration* is
   what drives cache-key trimming — a rule that consumes no axes (Nickel) produces output
   shared across every configuration.

6. **Is the hermeticity contract.** The rule decides precisely which inputs, outputs, and
   snapshot paths each action declares. Under-declaration → a stale input that isn't in the
   cache key → silently wrong output (the §1.4 cardinal sin). Everything else rests on the
   rule getting this set right (`docs/sandboxing.md`).

7. **Decides cacheability — but only as a *derived, enforced* property, never a claim.**
   See §4. This is the obligation the `pnpm_workspace` design stressed hardest.

8. **Defines the user-facing schema.** `schema()` is the rule's public API — the BUILD-file
   vocabulary (`cargo_workspace(name=…, data=…)`). A rule is also a language surface, and
   schema validation at the boundary (§4.3) is where user error becomes a clean diagnostic.

**The purpose, in one sentence:** a rule is the adapter that lets one opaque foreign
ecosystem participate in a single hermetic, content-addressed, configuration-aware graph
*without being decomposed* — exposing a narrow interface (providers + actions) while hiding
the engine's complexity. It is Ousterhout's deep module applied to an entire build tool.

## 3. The inference↔declaration spectrum

The single most useful idea this conversation produced. **How much a rule can automate is
set by how much fixed structure the wrapped ecosystem has** — and rules therefore sit at
different points on a spectrum from "the user declares everything" to "the rule infers
everything."

```
declare everything            declare some                infer almost everything
   genrule           ←      pnpm_workspace          ←        cargo_workspace
(user states inputs,     (resolution/install inferred;   (Rust/Cargo structure is
 command, outputs)        build/test scripts and           rigid; the rule reads it
                          their outputs declared)           all: members, lib targets,
                                                            test types, target/ cache)
```

- **`cargo_workspace` sits near the *infer* end** because Rust is unusually structured. The
  rule *knows* `cargo build` emits `.rlib`s at known paths, that `target/` is the
  incremental cache, that `cargo test --lib` exists. Cargo's verbs have fixed semantics, so
  the rule can be opinionated and automatic.

- **`pnpm_workspace` sits in the middle** because pnpm is a *package manager + script
  runner*, not a toolchain. `pnpm build` / `pnpm test` mean *nothing intrinsic* — they run
  arbitrary `package.json` scripts (tsc, vite, vitest, nothing). The rule can infer the one
  genuinely-pnpm-owned thing — dependency *resolution* — but the build/test scripts and
  their outputs are opaque, so they must be **declared** (see `docs/pnpm-workspace.md`).

- **`genrule` sits at the *declare* end** — the user supplies inputs, command, and outputs
  directly. It is the limiting case: zero structure assumed.

The practical lesson: **do not make a rule more opinionated than the ecosystem's structure
justifies.** `cargo_workspace` earns its automation; forcing the same "guess the build verb,
guess the outputs" automation onto pnpm would be a *lie about the ecosystem* that produces
silent correctness bugs. When an ecosystem is unstructured, the honest rule is more
declarative, and the messiness surfaces as explicit attributes rather than hidden guesses.

## 4. Cacheability: hermeticity is not determinism

A rule decides whether an action is cacheable, but for an engine whose behavior the rule
*cannot know* (any script-runner), "cacheable" can be neither asserted by the rule author
(who lacks the information) nor claimed by the user (who would be asserting a property the
system blindly trusts — the foot-gun). The resolution is to split the question into two
properties that are usually conflated:

- **Hermeticity** — the action reads *only its declared inputs* (+ toolchain, scrubbed env,
  no network). This guarantees the cache **key** is complete: a changed input is never
  missed. **Sealing enforces this** (strictly on Linux by construction; best-effort on
  macOS — `docs/sandboxing.md`).

- **Determinism / reproducibility** — the same inputs yield the same output *bytes*.
  **Sealing does not enforce this, and nothing the sandbox does can.** A sealed action can
  still embed a timestamp, a `Math.random()` seed, a per-build hash, or hashmap-iteration
  order into its output.

Caching a *hit* reuses a *prior* output instead of producing a new one. That is correct
only when the action's nondeterminism is **semantically irrelevant** (the §1.4 invariant:
caching may change *which valid output* you get, never the *semantic result*). A timestamp
in a comment is fine to reuse; a UUID that downstream depends on being unique is not — and
sealing cannot tell the two apart.

Therefore the correct cacheability rule is **not** "sealed → cacheable." It is:

> **sealed → the key is trustworthy.**
> **sealed *and verified reproducible* → the output is safe to cache.**

Reproducibility is *falsifiable, not provable* — a critical distinction. Building the sealed
action twice and byte-comparing (`verify_correctness_neutral`) is a test with **one-sided
error**: a *diff* is decisive (definitely non-reproducible → reject), but *agreement* is only
evidence ("didn't diverge in N samples"), never a proof. Most nondeterminism is caught even by
two builds — a wall-clock timestamp differs between runs; an entropy-seeded RNG differs every
time. The gap is **scheduling-dependent races**: output that depends on which thread finished
first, on `readdir` order, on an ASLR-seeded hashmap — which produce the canonical result
almost always and flip rarely under scheduler jitter. No finite sample closes that hole; it is
the flaky-test problem one level up.

So there are two categorically different responses:

- **Measure and accept bounded risk** — sample more (N randomized rebuilds); residual risk
  shrinks with the race's rarity but never reaches zero. Cheap, probabilistic, cross-platform.
- **Remove the *sources* of nondeterminism** so reproducibility holds *by construction* — then
  you don't sample. `SOURCE_DATE_EPOCH` (kill the clock), fixed PRNG seeds, and at the extreme a
  **deterministic-execution sandbox** (e.g. Meta's *Hermit*, which serializes thread scheduling
  and virtualizes time/randomness via syscall interception — Linux-x86-64 only, slow because it
  serializes concurrency, and in maintenance mode). We already practice the cheap end of this:
  `CARGO_INCREMENTAL=0` exists *because* rustc incremental codegen is not bit-stable; we disabled
  the non-reproducible mode rather than gamble on sampling.

The practical posture: the cacheability gate is **off the hot path** — a verification job, not
the executor — and may offer both strengths (cheap N-build sampling by default; deterministic
re-execution for the rare action whose scheduling-race risk genuinely matters). Note also that
bit-reproducibility is a *stronger* proxy than the §1.4 semantic-neutrality we actually need:
agreement on bytes is sufficient to cache, but byte-divergence does not by itself prove the
difference is *semantic* — which is why "remove the variance" is cleaner than "judge the
variance" when the cost is justified.

This reframes obligation 7 precisely: **a user never declares "cacheable." A user declares a
*constraint* (`sealed`) that the system enforces, and the system *derives* cacheability by
*verifying* reproducibility.** The user's declaration makes the key honest; the harness makes
the value safe. The safe default is therefore **non-cacheable**, because reproducibility is
unproven until measured — and non-cacheable is cheap, because the engine's own incremental
state is restored via snapshot (see below), so a non-cached action still re-runs fast.

**No consumer cacheability foot-gun — graduation is a system action.** Pushing this to its
conclusion: a rule must expose **no knob a consumer can use to assert cacheability**, because
that would be the unverifiable claim that poisons the cache. Three properties keep this airtight:

1. **There is no `cacheable = true` attribute.** A consumer cannot declare a result reproducible.
2. **A consumer's levers move only *toward* non-cacheable** — e.g. marking an action `permeable`
   (needs network) makes it *definitely* non-cacheable. There is no lever in the other direction.
3. **`sealed` is safe to expose even though it's a consumer-expressible constraint**, because it
   is *enforced*, not trusted: a sealed action that attempts an undeclared read or network
   **fails loudly** — it can break a build, but it cannot silently produce a wrong cache entry.

So "graduating" an action to cacheable is never a consumer assertion. The engineer's job is to
make the action *deserve* the cache — declare its inputs/outputs honestly so it can run sealed,
and remove nondeterminism (`SOURCE_DATE_EPOCH`, fixed seeds) so it is reproducible. The
**system** then grants the cache only after its off-hot-path verification *observes* byte-identity.
The rule author ships the enforcement and the gate; the consumer expresses intent and honesty;
the system is the sole grantor. (The one residual hole is platform — best-effort sealing on
macOS — but it is the *existing* posture that already applies to cargo, and no consumer knob can
widen it.)

## 5. Two kinds of cache, and what "snapshot" means

A rule has two distinct correctness-neutral accelerators (§8), and confusing them causes
trouble:

- **The action cache** (§8.1) — maps a complete action *key* to a result, and a *hit skips
  execution entirely*. Sound only under the §4 reproducibility condition. This is the
  optimization that avoids the engine's startup cost altogether.

- **The snapshot** (§8.2) — restores the engine's mutable *incremental state* (`target/`,
  `.tsbuildinfo`, `node_modules/.vite/`) into the sandbox before a run and saves it after. A
  snapshot **never skips the action**; it makes the re-run fast. Governed strictly by §1.4:
  restoring it may only change *speed*, never *output* — gated by the same double-build
  neutrality harness. If an engine's incremental mode is *not* output-neutral, the rule
  disables it (the `CARGO_INCREMENTAL=0` move) and snapshots nothing.

The combination "**non–action-cacheable + snapshot-consuming**" is the honest default for
an opaque script: we never trust a skip, but we still restore the engine's scratch state so
the unavoidable re-run is cheap. The only thing a true action-cache hit buys *over* this is
eliminating engine startup — a real win for slow-to-start tools (tsc), but an *optimization*
that must pass the reproducibility gate to turn on, not a default anyone backs into.

### Two snapshot policies: `SnapshotBased` (earned) vs. `SnapshotConsuming` (default)

The kernel encodes that default as a `CachePolicy` distinct from cargo's. Both restore a
correctness-neutral snapshot (the §1.4 floor holds for either — that is *not* the difference);
they differ on two orthogonal axes:

| | `SnapshotBased` (cargo, pnpm `install`) | `SnapshotConsuming` (pnpm scripts) |
|---|---|---|
| **Action-cacheable?** (skip via a recorded result) | **yes** — output is verified reproducible | **no** — output not trusted reproducible; always re-runs |
| **Snapshot ownership** | **writer** — restores *and saves* (co-maintains the cache, neutrally) | **reader** — restores a snapshot another action owns; never saves |

- **Primary axis — reproducibility → cacheability.** `SnapshotBased` may *skip* the action
  (reuse a recorded output); this is sound only because the action is verified reproducible. A
  script's output (timestamps, build IDs, randomness) is not trusted, so `SnapshotConsuming`
  *never* skips — it restores the snapshot only to be able to *run*.
- **Secondary axis — read vs. write.** A `SnapshotBased` writer must keep the shared snapshot
  neutral (the `CARGO_INCREMENTAL=0` discipline). A `SnapshotConsuming` reader takes a snapshot
  it does not own (a script reading `install`'s `node_modules`) and **does not save** — so it
  cannot corrupt the shared snapshot; read-only sidesteps the trust question structurally.

**Why "consuming," not "accelerated."** For `SnapshotBased` the snapshot is a true *accelerator*:
delete it and the owning action cold-rebuilds the *same* result, only slower. For
`SnapshotConsuming` the restored snapshot is a **necessary input the action cannot re-derive
itself** — a script with no `node_modules` doesn't run slowly, it *fails*. The build as a whole
stays correctness-neutral (§1.4) only because the snapshot's **owner** (`install`) runs first and
re-derives it when absent — so neutrality is a property of *owner + consumers together*, not of a
consuming action in isolation. (One coherence requirement falls out: the owner's snapshot must be
*present* when consumers run. An owner that **action-cache-hits with an empty snapshot store**
returns early without re-saving, breaking its consumers — a real hazard for shared/remote caching,
benign on a single machine where the owner populates it once.)

They are **stages, not castes.** `SnapshotConsuming` is the correct policy *until* reproducibility
is proven; the deferred verification gate is the promotion path to (effectively) `SnapshotBased`.
And the two axes are genuinely orthogonal — `SnapshotConsuming` bundles {non-cacheable, read-only}
because that is exactly the script case, but a fully general design would treat *cacheable?* and
*saves?* as independent. The bundling is a known simplification, not a hidden one.

### Snapshot vs. content-addressed Output — re-derivable, not deletable

It is tempting to think a snapshot is "data the action doesn't really need" — but the
incremental state is often genuinely necessary for the action to *run* (`tsc` cannot compile
without `node_modules`). That feels like it blurs the line between identity and acceleration.
It does not, once two questions are kept apart:

- **What does the action's *result* depend on?** → its **identity** (the cache key).
- **What bytes must be *present* for it to run?** → its **availability** (delivery into the sandbox).

A snapshot is firmly on the *availability* side, never the *identity* side, **whenever its
content is a pure function of a smaller, cleaner key.** `node_modules` is a function of the
lockfile (+ toolchain + platform); `target/` is a function of sources + toolchain. So the
correct §1.4 statement is not "deletable with no effect" but **"re-derivable with no effect on
the result"**: delete the snapshot and the system rebuilds it from the identity (e.g. `pnpm
install --frozen-lockfile`), then runs — output unchanged, only slower. Cold-start is handled
by rebuild, not by breakage.

The decisive test for **snapshot vs. content-addressed Output**:

> **Can I reconstruct this from a smaller semantic key?**
> **Yes → snapshot** — key on the small thing (lockfile, toolchain version), deliver the bytes,
> re-derive on a miss. Non-determinism is *survivable* (it only costs hit-rate).
> **No → content-addressed Output** — the content *is* irreducible identity (a generated
> `config.json` can't be recovered from a tiny key; you'd have to re-run its producer), so its
> content must *be* its address. Here non-determinism is *fatal*: digest churn means it never
> hits, and feeding incremental state as an Output would leak it into identity (the §1.4
> under-invalidation foot-gun).

This is the same relationship as a **toolchain**: `rustc` is necessary-for-execution too, yet we
key on its *version* and deliver the binary, rather than hashing it into every action. Necessary
data identified by a coarse key and delivered into the sandbox is the snapshot pattern, not the
Output pattern.

## 6. Checklist for designing a new rule

1. **What does it wrap, and where is the fixed structure?** Locate the rule on the
   inference↔declaration spectrum (§3). Infer what the ecosystem makes knowable; require the
   user to declare what it does not.
2. **What does it expose upward?** Define the providers before the actions (§2, obligation 2).
3. **What does it own?** One package subtree, exclusively (obligation 4).
4. **Which axes does it consume, and how do they map?** (obligation 5; §13.6.)
5. **What exactly are the inputs, outputs, and snapshot paths?** This is the hermeticity
   contract — get it complete (obligation 6; `docs/sandboxing.md`).
6. **What is cacheable, and how is that *earned*?** Default non-cacheable; cacheability is a
   *derived, reproducibility-verified* property of a *sealed* action, never a user claim
   (§4).
7. **What is the BUILD-file schema?** Its public API (obligation 8).
