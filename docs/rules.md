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
analyze(ctx) -> Analysis { actions, providers, routed_data }
```

**A rule is a pure function from a configured-target context to a slice of the action
graph plus a set of providers.** That pair is the entire *semantic* contract — `actions` are
the work, `providers` are the interface upward. The third field, `routed_data`, carries no
build semantics; it is a materialization affordance derived from the actions (broken down
below). Everything else in this document is an *obligation* or a *freedom* of that function —
not a separate mechanism.

Two consequences fall directly out of "pure function, run in the analysis phase":

- It **cannot execute anything.** It emits actions to be run later; it does not run them.
- It **cannot see generated content.** It runs in analysis, before execution, so any
  artifact a downstream action will produce does not exist yet. A rule reads *source* and
  *static declared structure* only. (This is the §14.6 phase wall — the reason a generated
  `Cargo.toml` / `pnpm-workspace.yaml` is impossible to consume as an edge, but a generated
  `config.json` is fine.)

### What `analyze` returns

`Analysis` has three fields, and they are deliberately *not* co-equal — two are the contract,
one is an affordance:

- **`actions` — the work.** A slice of the action graph: coarse units the engine schedules,
  keys, sandboxes, and runs (the rule never runs them — the phase wall above). This is *what
  gets done*, and it is the sole thing that determines the build's outputs.
- **`providers` — the interface offered upward.** What this target exposes to anything that
  depends on it (`FileSet` today; the broader typed-provider vocabulary — `TestSuite`,
  `LibraryInfo`, … — is `build-system-design.md` §5.5). Providers flow *up*, configuration flows
  *down* (§5.4). Routing a generated artifact across a language boundary is entirely a
  provider/consumer story (§14): a `nickel_eval` exposes its JSON as a provider; a consumer
  picks it up. This is *what the target offers*.
- **`routed_data` — the consumer-side materialization map.** The generated files *this*
  target's actions consume at tree-shaped paths — the resolved `data` routing, each artifact's
  `path` being the package-relative spot the inner tool reads it as if it were a source. It is
  **not new information**: the dependency already lives in `actions` as an input edge. This field
  re-surfaces *which generated inputs land at which working-tree paths* so `anneal materialize`
  can mirror the sandbox's input view into the developer's working tree for native tools
  (`cargo run`, rust-analyzer). Drop it and the build is byte-identical — only `materialize`
  loses its map. It excludes sources (already in the tree) and sandbox plumbing (fetched
  `.crate` blobs, vendor assembly); most provider-only rules leave it empty.

The asymmetry is the point. `actions` + `providers` define the build; `routed_data` only lets a
tooling command reconstruct what a build *sees*. A rule that gets `routed_data` wrong yields a
worse `materialize` experience, never a wrong build — which is exactly why it sits *outside* the
§6 trust-boundary's correctness duties.

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

### How the snapshot key is formed — conservative by construction

The snapshot's *content* is re-derivable (above); its **key** — "which warm tree is safe to
reuse here?" — is engine-derived and deliberately conservative. It keys on everything that
could make reuse unsafe (toolchain identity, the lockfile digest, the target triple, the
consumed axes, any native-library identities — `cargo_workspace.rs::target_state_shard`) and
excludes only the one thing the wrapped tool absorbs incrementally: the workspace **source
tree**. A source edit is caught by the *action* key (every source file is a declared input)
and merely absorbed by the tool's own fingerprinting inside the warm tree; a toolchain / dep
/ flag change re-keys the snapshot and forces a cold tree.

There is deliberately **no "incremental-safe" exclusion verb.** A rule cannot tell the engine
"this input or env doesn't affect the warm tree, leave it out of the key." The only
incrementality judgment the API admits is coarse and categorical — *which directory is the
wrapped tool's incremental domain* — and it is carried by the **declaration channel**
(`source_tree(".")` = the tool's incremental domain → excluded; explicit `input()` / lockfile
/ `data` → keyed), never by a per-input flag. Conservative-by-default falls straight out of
the vocabulary: anything not routed through the tool-domain channel is keyed, so a rule errs
only ever toward *more* cold rebuilds (a performance cost) — never toward an under-keyed warm
tree (a §1.4 violation).

This is the cacheability foot-gun stance (§4) one level down. A finer incrementality claim
would be a **cost-free, engine-unverifiable trust with no revocation lever** — precisely the
shape the design refuses everywhere (a consumer's levers move only *toward* non-cacheable;
cache *tier* may be restricted, never escalated). And it buys nothing legitimate: every input
a rule might want to mark incremental-safe is either the source tree (already excluded), an
output-relevant input (excluding it *is* the cardinal sin), or an output-irrelevant input
that churns — and the last is already an anti-pattern poisoning the *action* key too, whose
correct fix is to stop feeding it to the action (fixing both caches). The verb has no honest
use and only foot-gun potential.

**Guaranteed vs. trusted is the pure-vs-stateful line.** A structural neutrality *guarantee*
exists for content-addressed accelerators — the action cache reuses an output keyed on its
inputs' content, neutral by construction, trusting no tool behavior. It does **not** exist for
the stateful snapshot, whose neutrality is a property of the wrapped tool's internal
incrementality: a counterfactual about the cold build the engine can neither observe nor
enforce, only sample (§4's one-sided harness). That line is the price of "wrap, don't
decompose" (`build-system-design.md` §3.2) — the only way to *earn* a structural guarantee for
the fine-grained layer is to own incrementality yourself (shatter the build into per-unit
content-addressed actions), which is the decomposition anneal exists not to do. So the
snapshot's neutrality is **trusted, not guaranteed**, and the design makes that trust safe
rather than pretending it away:

- **Minimized** — keyed conservatively, the warm tree absorbs only source edits, so the trust
  reduces to one universally-relied-on property: the wrapped tool recompiles changed sources
  correctly. (anneal in fact trusts the tool *less* than its own default does — a developer's
  `target/` survives toolchain and dep changes too, leaning on cargo's fingerprint for all of
  them; anneal re-keys cold on those.)
- **Contained** — snapshot owners are capped at the `Local` cache tier
  (`anneal-exec/src/trust.rs`), so warm-derived outputs are never promoted to the
  shared/remote cache, which is populated only by the cold Hermetic arm (content-addressed,
  full identity). A neutrality violation therefore poisons one machine — recoverable with
  `anneal clean` — and never the team.
- **Revocable** — the attestation epoch folds into the state key (`anneal-rules/src/state.rs`),
  so a discovered tool-incrementality bug is retracted by one constant bump that mass-
  invalidates every warm tree derived under it. You cannot *prevent* the violation; you can
  globally *withdraw* the trust once it is known.

*(Known residual, tracked in `TODO.md`: env enters the action key but not yet the snapshot
shard, so a build script that reads an env var without declaring `cargo:rerun-if-env-changed`
could reuse a stale warm tree on an env change. `cargo_workspace` is currently safe — its
action env is either closure-derived, hence captured via toolchain identity in the shard;
axis-derived, hence in the shard; or constant — but the structural fix, folding author-added
env into the snapshot key or forbidding it on snapshot owners, is open work.)*

## 6. The trust boundary — what a rule can and can't enforce

> The specific ruling behind this section — *no incremental-safe exclusion verb*, the
> conservative engine-derived snapshot key, the `Local`-cap containment, and the alternatives
> weighed and rejected — is recorded in
> `docs/decisions/0001-snapshot-keying-and-the-rule-trust-boundary.md`.

Everything above resolves into one picture: a rule makes the engine a handful of promises, and
the engine's safety rests on knowing, for each, whether it can **enforce** the promise,
**derive** it, only **verify** it as a sanity check, or merely **trust** it — because that is
exactly what decides what happens when a rule gets it wrong.

A rule author therefore has **two correctness duties, and they are orthogonal — neither
backstops the other:**

- **Input completeness** (the *action* key): declare every input that affects the output, and
  feed the action nothing output-irrelevant. This governs *"do we re-run when something that
  matters changed?"* It is **enforced** — the sandbox mounts exactly the declared inputs, so an
  undeclared read fails (loudly on Linux by construction; best-effort on macOS,
  `docs/sandboxing.md`). Under-declaration is the §1.4 cardinal sin, but enforcement converts it
  from a silent stale output into a build failure.
- **State neutrality** (the *snapshot*): restoring the warm tool-state must never change the
  output versus a cold build. This governs *"is a warm re-run the same as a cold one?"* It is
  **not** enforceable — it is the wrapped tool's internal property (§5), so it is trusted,
  sampled by the double-build harness, contained by the `Local` tier cap, and revocable by the
  attestation epoch.

They do not substitute for each other. A complete action key does **not** rescue a non-neutral
snapshot: the changed input correctly triggers a re-run, but that re-run reuses the stale warm
tree and then *caches the wrong output under the now-correct key*. A neutral snapshot does not
rescue an incomplete action key: the action never re-runs at all. Both duties are independently
necessary — the lockfile sits in **both** keys for exactly this reason (§5).

The full map of what the engine does with each rule-shaped promise:

| Promise | Engine's stance | Mechanism | If the rule is wrong |
|---|---|---|---|
| Declared inputs are complete | **Enforced** (Linux) / best-effort (macOS) | sandbox mounts only declared inputs; undeclared read fails | Loud failure (enforced); silent stale output where best-effort — the §1.4 sin |
| Output is reproducible | **Verified** — falsifiable, not provable | off-hot-path double-build byte-compare, one-sided error (§4) | Stays non-cacheable (the safe default) until observed reproducible |
| Result is cacheable / its cache tier | **Derived, never claimed** | engine computes; rule/consumer levers move only *toward* non-cacheable / a *lower* tier | Cannot err in the unsafe direction — there is no lever toward more-cacheable |
| Snapshot key is complete | **Engine-derived, conservative** | everything keyed except the tool's incremental domain; no exclusion verb (§5) | Over-key → cold rebuild (performance); under-key is structurally hard to express |
| Snapshot is neutral | **Trusted** + sampled + contained + revocable | tool's own incrementality; harness samples; `Local` cap; epoch revokes | Silent wrong output — but quarantined to one machine, recoverable, never the shared cache, revocable |
| State directories are complete | **Trusted** (a rule declaration) | rule names the interleaved-state paths | Incomplete set → a non-neutral snapshot (folds into the row above) |

The principle the table makes explicit — and the one to hold while authoring a rule — is that
**the API is deliberately shaped so a rule cannot make an engine-trusted claim which, if wrong,
silently miscaches.** Every rule- and consumer-expressible lever moves toward the *safe* (more
conservative) side — non-cacheable, lower tier, more cold rebuilds — and none toward the
dangerous side. The single trust that *cannot* be designed away (snapshot neutrality, because it
is the wrapped tool's property and erasing it would mean abandoning "wrap, don't decompose") is
not left bare: it is minimized to one well-understood property, contained to a single machine,
and made revocable. So a rule author's job is not to *assert* correctness to the engine — the
API mostly forbids that — but to *earn* it: declare inputs and state paths honestly so the
enforced and derived machinery can work, and remove nondeterminism so the verifier can graduate
the cache.

## 7. Checklist for designing a new rule

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
8. **Re-read §6: is every promise your rule makes one the engine can enforce, derive, or
   contain-and-revoke — never a bare trusted claim that silently miscaches if wrong?** If your
   rule needs to *assert* a correctness property to the engine (rather than declare inputs/state
   honestly and let the machinery work), treat that as a design smell and find the conservative
   formulation instead.
