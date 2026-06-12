# Anneal Design Document

**Status:** Living document, v2 — distilled June 10, 2026, from an extended design conversation and three rounds of review of rule-API sketches (v1–v3); amended June 11, 2026, after a fourth review round (a review of this document itself), the macOS/execution-platform exploration, and the first-afternoon product spec. This doc is the arbiter when the code, the sketches, and memory of the conversation disagree. `build-system-design.md` remains the detailed Milestone-1 reference for surfaces this doc doesn't cover (CLI grammar, BUILD-language details, diagnostics schemas); where the two disagree on a matter this doc decides, this doc wins. Each section records the decision, the rationale, and where relevant the rejected alternative — the rationale is the point; conclusions without their arguments get relitigated.

**Changes from v1 (round-4 review, resolved):**

1. **The bootstrap cycle is fixed.** v1 shipped a demand cycle as two separately-correct decisions (§3.6's metadata-query-reads-registry plus §8's fetch-includes-from-metadata). Resolution: the two-query bootstrap split, plus registration-finality semantics, plus state edges in the demand-cycle checker (§3.6, §5.1).
2. **Enforcement is platform-graded** (§2.8, §7). `Enforced` vs `LoudBestEffort` are typed properties of execution platforms; the tier table gains a row; macOS stops being an unprinted asterisk and becomes a queryable property — the same move as `Attestation`. macOS support is hybrid: native Seatbelt for darwin-target actions, an anneal-managed Linux VM (post-M1) for linux-target actions.
3. **The first afternoon has a spec and a gate** (§1.5). §1 declared the adoption-cost curve the product; v1 contained no section about it.
4. **The agent-timing argument engages its counterargument** (§1).
5. Smaller: latency claims tied to persistence rungs (§6.4); query byte-determinism's dependency on sandbox path stability added to the spike list (§10); test-result caching is a named deferral (§9); cone mis-coloring honesty sentence (§4.2); the round-4 process lesson recorded — the checklist applies to amendments too (§10).
6. **Appendix A added** (code/design reconciliation): a three-agent audit of the codebase against this doc found five places where code and design embody *different mechanisms*, not just different maturity. Each now has a recorded ruling, and the convergence sequencing is decided: §10's spike immediately, M1 remainder in parallel, then in-place evolution.

---

## 1. Positioning and thesis

Anneal is a hermetic, polyglot build system that **wraps native ecosystem tooling** (cargo, pnpm, go, nickel) rather than replacing it, targeting small-to-mid-sized polyglot monorepos — roughly 5–50 engineers with at least one correctness-sensitive seam (cross-language codegen, native deps, reproducibility requirements).

The gap is real and persistently unfilled. The tier above (Bazel, Buck2) demands full conversion: BUILD files everywhere, native toolchains abandoned, a perpetual rules_* maintenance treadmill chasing upstream tool semantics. The tier below (Turborepo, Nx, Justfile piles, path-glob CI filters) has near-zero adoption cost but shallow hermeticity — hash-based caching with no isolation, so undeclared inputs poison caches silently and people learn to `--force`. The middle has a graveyard: Earthly (shut down 2024 — container-grain wrapping too coarse to compete with native inner loops), Please (technically excellent, plateaued — its incremental-adoption story was rhetorical, the first afternoon still ended in "write BUILD files for everything"), Pants (struggled). The graveyard's lesson is that **the adoption-cost curve, not the feature set, is the product**: build-system value is diffuse (five minutes at a time, forever) while conversion cost is concentrated, and small companies rationally defer diffuse payoffs.

The core technical insight that distinguishes Anneal from the graveyard:

> **Enforcement over declaration.** You don't need fine-grained input *declaration* to get trustworthy caching — you need fine-grained input *enforcement*. The sandbox makes undeclared inputs unreadable rather than merely unhashed, so an action that *succeeds* has proven its input set complete. Coarse wrapping costs cache *hit rate*, never cache *correctness* — and correctness is the property the lightweight tier has never delivered.

(Where the platform cannot fully enforce, the claim is graded rather than silently weakened — §2.8. The strong form above holds on `Enforced` platforms; the trust model makes the grade explicit everywhere else.)

This is what makes structural partial adoption possible: one package under management, the rest of the repo untouched, with the half-converted graph degrading to coarse-but-correct rather than fine-but-wrong (see the refinement ladder, §3.7).

The features compose as one chain, and they cannot be sold à la carte: toolchain pinning + sandboxing make caching trustworthy, which makes test-result caching believable, which makes affected-target CI honest. Bazel's insight was that these are one feature wearing eight costumes; the lightweight tier's failure mode has been picking off costumes individually.

**Timing argument:** agentic coding changes who the user is. Agents don't read READMEs, can't be trusted with undeclared host access, and amplify flaky-cache problems by orders of magnitude. A build system whose every action is sandboxed, whose inputs are enforced, whose graph is queryable, and whose caches are sound is the substrate agent-driven development needs — and that positioning did not exist for any of the graveyard projects. This converts Anneal's diffuse value into an acute one, and it is the one live path past the base rates.

**The counterargument, engaged rather than ignored:** agents also collapse the conversion cost of the incumbent tier — writing BUILD files everywhere and chasing the rules_* treadmill is exactly the mechanical, well-documented toil agents do well, so the force cited as tailwind also lowers Bazel's adoption-cost curve. The rebuttal: cheap BUILD authoring doesn't change what declaration-based correctness *is*. Bazel's guarantees still rest on declared inputs at the native-tool boundary, and an agent-authored declaration is exactly as trustworthy as the agent's diligence that hour — enforcement is the property that doesn't depend on the author's care, which matters more, not less, when the author is a machine. Agents amplify cache distrust regardless of who wrote the BUILD files, and an agent-maintained rules_* fork is a compounding liability, not an asset. If conversion cost falls everywhere, correctness becomes the remaining differentiator — which is the bet.

**Honest odds, recorded so they aren't forgotten:** the modal outcome for this category is "respected, used by hundreds of discerning teams" rather than breakout. The central technical bet (§2.2, managed mutable tool state) has a demo gate: cargo incremental state, hermetically managed, with inner-loop times within shouting distance of bare cargo. If that demo works, the rest of the thesis follows; if it doesn't, no amount of Starlark ergonomics rescues it.

### 1.5 The first afternoon

§1 declares the adoption-cost curve to be the product; this section is that product's spec. It is **normative**: changes that degrade the sequence below are product regressions even when they improve the internals, and the sequence is a benchmark gate alongside the inner-loop demo.

**The gate:** an existing cargo or pnpm monorepo goes from `anneal init` to a green hermetic CI build **plus one shared-cache hit on a second machine, within one afternoon, without editing any source file.** Files created: `WORKSPACE`, one `BUILD`, one CI config block. Files touched: nothing else — Anneal owns `.anneal/` and claims no native tool directory (`build-system-design.md` §15.3).

**Minute 0–5.** Single static binary, no runtime deps. `anneal init --detect` scans for `Cargo.toml` / `pnpm-workspace.yaml`, scaffolds a `WORKSPACE` and one package-level `BUILD` invoking `cargo_workspace` / `pnpm_workspace`, and prints what it inferred (toolchain version from `rust-toolchain.toml`, profile, member list). The generated BUILD is the restricted Starlark subset (`build-system-design.md` §4.2): rule invocations and literals, nothing a non-Bazel-user can't read.

**Minute 5–15: the first build, and the honest sentence.** The first `anneal build` is cold: toolchain provisioning plus ingestion plus a cold cargo build. The CLI says so — "first build provisions and proves; warm builds are the product" — because an unexplained four-minute first build is where trust dies. Subsequent builds run warm (§2.1 interleaved state, warm reuse), and the second invocation after a one-line edit is the demo-gate number.

**Minute 15–60: the first sandbox violation — the defining interaction of the product.** Enforcement produces a failure class declaration-based tools never surface, and whether it reads as *caught bug* or *fighting the tool* decides adoption. Three commitments, in priority order:

1. **Every violation names the path, the action, and the declaration that fixes it.** "`CargoBuild //app` read `.env`, which is not in its source set" arrives with the exact `srcs` edit that would declare it, as a structured error (`build-system-design.md` §17: stable code, `anneal explain` long form). Read-tracking exists for this diagnostic (`docs/sandboxing.md` §4): on Linux it upgrades `ENOENT` to a named accusation; on macOS it is the enforcement.
2. **Every violation explains why it's a catch, not an obstacle.** One sentence of the form: "this file is absent on CI and on your teammates' machines — without this failure, this build would have silently differed across machines." The justification is the product's thesis delivered at the moment of friction; omitting it leaves only the friction.
3. **Remediation is one suggested command, never auto-applied.** The suggestion may be wrong in spirit (the right fix for `.env` may be *stop reading `.env` in builds*), so the diagnostic offers the declaring edit and names the alternative.

The common violations are enumerable and few — `.env`, toolchain-manager files (rustup overrides), `~/.gitconfig`-driven behavior, `build.rs` network access, postinstall scripts — and each ships a curated diagnostic, maintained as a test corpus (**violation vignettes**: each vignette is a fixture repo, an expected structured error, and an expected suggested fix, compiled in CI like the rule-API examples — the §10 compiled-example discipline, applied to diagnostics).

**Hour 1–4: the wedge, then the ladder.** The wedge is CI caching (`build-system-design.md` §15.1): one CI config block, CI populates the shared cache from Hermetic-mode builds, a colleague's `anneal build` hits it — the gate's second machine. From there, partial adoption is the refinement ladder (§3.7) experienced as optional performance work, never required correctness work: unmanaged deps default to `OpaqueTree` (coarse, sound), `anneal query` shows where the coarse edges cost hit rate, and each refinement is a measurable hit-rate purchase. Nothing in the afternoon requires wrapping the second package; that's the structural-partial-adoption claim made operational.

**What the afternoon does not promise, said out loud:** test-result caching arrives only when tests run under management (§9); macOS-native results don't populate the shared cache (§2.8); the first build is slower than native and the tenth is the comparison that matters. Each of these is stated by the CLI at the relevant moment rather than discovered by the user as a gap.

---

## 2. The trust model

### 2.1 Two kinds of tool state

Wrapped tools carry their own mutable caches, and hermeticity must coexist with them rather than destroy them (exclude them and every build is cold; admit them naively and they're undeclared mutable inputs). The taxonomy:

**Phase-separated state** is produced by exactly one action and consumed read-only by others: node_modules, the crates.io registry, Go's module cache, uv venvs. The framework *enforces* every invariant mechanically — read-only bind mounts for consumers, producer identified by `StateUse::Produce`, the tree tracked by its producing action's key rather than content-ingested (it exists as a kind distinct from output artifacts because tool-shaped trees — symlink farms, store hardlinks — aren't worth normalizing into the CAS). No trust is delegated; no attestation exists.

**Interleaved state** is mutated by the very actions that read it: cargo's `target/`, GOCACHE, `.tsbuildinfo`. Correctness rests on the wrapped tool's *internal* invalidation logic, which the sandbox cannot inspect. Declaring this kind **is** the rule author vouching for it: it demands an `Attestation { epoch, rationale }`, and every mutating action is capped at `CacheTier::Local` — so a bad attestation poisons one laptop (recoverable by `anneal clean`), never the team's shared cache. This single policy converts the scary version of trust delegation into the tolerable version while preserving the inner-loop speed that motivated the design.

Prefer phase separation wherever the ecosystem allows it. Interleaved is the concession, not the default.

### 2.2 The central technical bet

Treating the tool's mutable cache as **declared, managed, content-journaled state** — mutable but never *undeclared* — is the bet the whole project rides on. It's the reason Bazel chose replacement (owning incremental state) and the reason Earthly lost to native inner loops (refusing to manage it). If managed-mutable-tool-state can be made both sound and fast, Anneal has something Please, Earthly, and Turborepo all lacked. If not, the fork is slow-and-correct vs. fast-and-Turborepo, which is the graveyard.

### 2.3 Reuse-soundness vs. byte-determinism: orthogonal axes

These were conflated early in the design and pulling them apart matters everywhere:

Property (a), **reuse-soundness**: reusing state never produces *wrong* outputs — staleness is always detected. This is the correctness requirement; the framework needs it absolutely.

Property (b), **byte-determinism** (`OutputContract::ByteDeterministic`): identical inputs ⇒ identical bytes. This is purely an optimization declaration. It unlocks early cutoff (a rebuilt-but-identical artifact terminates the invalidation cascade), cross-machine cache convergence, and cheap byte-level differential audit. Misdeclaring it costs spurious audit alarms and lost cutoff — never a wrong build, which is what makes it safe to let rule authors declare it.

The rule-author contract is therefore: you *must* guarantee (a) for any state you declare; you *should* declare (b) where you provide it. Cargo gives (a) under a constrained configuration and (b) only with incremental off + path remapping; Nickel gives both for free; the gradient is the per-ecosystem reality (§8).

### 2.4 Cache tiers: computed, never declared

```
any StateUse::Mutate                                  ⇒ at most Local
NetworkPolicy::Allowlist                              ⇒ at most Local (no exceptions)
enforcement < Enforced (§2.8)                         ⇒ at most Local
ByteDeterministic ∧ Denied ∧ no Mutate ∧ Enforced     ⇒ Promotable
ReuseSound (otherwise clean)                          ⇒ Promotable; no early cutoff;
                                                        set-level (not byte) audits
```

The tier is a function of **(spec, execution platform)**, not of the spec alone — the same action key can be Local-capped on a Mac laptop and Promotable in CI; the difference is provenance, not identity (§2.8). Rule authors may restrict the computed tier (`ExecPolicy::max_tier` — the escape hatch for Gradle-shaped wraps whose sound configuration cone is too narrow to certify); they may never escalate. The dev loop and the shared cache are reconciled by configuration (§4): the same rule emits a warm `Mutate` action under `Incremental` and a cold `ByteDeterministic` action under `Hermetic`, CI populates the shared cache from the latter, and the tier system arbitrates which result is trusted where.

**History worth keeping:** v1 of the rule-API sketch contained a network-promotion exception ("promotable if every fetched byte is pinned by integrity hashes in declared inputs"). Review round 2 correctly identified it as an *unmarked attestation* — a rule-author-vouched invariant the framework can't verify, with none of the attestation machinery. The resolution was **deletion rather than patching**: interrogating the cases that seemed to need it found none (toolchain provisioning is framework-verified against declared digests; fetch rules produce phase-separated state tracked by producer key, never content-promoted). If a legitimate case ever emerges, the mechanism is sandbox-recorded fetch digests checked mechanically against declared pins — never a quiet claim.

**That mechanism already exists in the code.** `CachePolicy::FixedOutput { expected: Digest }` (`anneal-exec`) is exactly the sanctioned shape: a networked fetch whose *output digest is verified mechanically by the framework* — the pin carries the key, checked, never vouched. The cargo rule uses it today for per-dependency crate fetches pinned to lockfile checksums. Absorbed as the blessed pinned-fetch path (Appendix A, ruling 2); fixed-output actions are cacheable because the verification is structural, which is consistent with — not an exception to — the no-quiet-claims rule above. Tier consequence, decided at the trust-plumbing landing: **fixed-output ⇒ `Promotable`, grade-independent** — the digest check is performed wherever the artifact lands, so the verification, not the producing sandbox, is the trust. This is the one row where a `LoudBestEffort` host produces a promotable result, and it is sound for exactly the reason the v1 network exception wasn't.

### 2.5 `Read` of interleaved state is forbidden

Round-3 soundness catch: a read-only consumer of interleaved state has an input that exists in no key (the contents are by definition not content-tracked), so it computes as Promotable while being silently stale-able — and even Local caching is unsound, since sibling mutations invalidate nothing. **Decision: forbid the arm entirely**, not the reviewer's fallback (a per-StateKey mutation generation folded into reader keys). The fallback is sound but self-defeating — every mutation invalidates every reader, so caching readers buys nothing over re-running — and no real use case exists: interleaved caches are read-write by every user (cargo test mutates `target/` too), and "read the target dir" is really "extract an output," which is the producing action's job. The sound answer is also the smaller API. Tier-table doc language: `Read` never affects tier *for phase-separated state*; for interleaved it does not exist.

### 2.6 Attestations and epochs

`Attestation { epoch: u32, rationale: &'static str }` is the signed-in-code acknowledgment accompanying interleaved state, surfaced by `anneal query --explain-trust`. The epoch exists so a discovered soundness bug — in the wrapped tool (cargo's 1.52.0 incremental miscompilation emergency is the canonical precedent) or in the rule — can mass-invalidate every entry derived under it. **Epoch is a per-rule-version constant**, never computed from attrs. State keys are implicitly scoped by the declaring rule's `NAME`, making cross-rule state sharing — and mutating under another rule's attestation — *inexpressible* rather than discouraged. Given that scoping, the `declare_state` mismatch error's honest job is catching shard-content drift between targets of one rule, not epoch conflicts between rules.

### 2.7 Audit: making the unenforceable detectable

Differential auditing is the framework primitive that makes the residual trust surface inspectable: sampled shadow-rebuilds of cache hits, executed cold in CI, compared against the cached result. Divergence triggers alarm plus **epoch quarantine of the offending state namespace — never silent repair** (silent repair hides the bug; quarantine surfaces it). Bazel's persistent workers are the precedent in both directions: the same trade (long-lived mutable state for speed, correctness delegated), and a decade of bugs caught mainly by exactly this differential checking. Audit policy lives in `ExecPolicy`, **excluded from action identity** — changing a sample rate must never invalidate caches. `ByteDeterministic` actions get byte-level comparison; `ReuseSound` actions get success/output-set comparison. The default sample rate derives from **tier × enforcement grade** (§2.8): results produced under weaker enforcement get proportionally more shadow-rebuild attention, because audit is precisely the mechanism for "we can't prove it, so we sample-check it."

### 2.8 Enforcement is platform-graded

The pillar-one claim — *an action that succeeds has proven its input set complete* — is a theorem only where the sandbox makes undeclared inputs structurally absent. Where it can't, v1 of this doc was silent, which was the round-4 catch: silence is an unprinted asterisk on the central claim. The resolution is the same move as `Attestation`: make the weaker trust **typed, loud, bounded, and sampled**.

The codebase already distinguishes what an action *requests* — `ExecutionMode::{Sealed, Permeable, Native}` in `anneal-exec` (`Sealed` is the only cacheable mode). The enforcement grade is the orthogonal axis: **what the platform actually delivers when an action requests `Sealed`.**

**`Enforced`** — undeclared inputs are *structurally absent*. Linux bubblewrap builds a mount namespace in which undeclared host files do not exist; a read fails `ENOENT` because there is nothing there to read. Action success proves input completeness, kernel-enforced. The Linux VM on macOS (§7.3) is also `Enforced`: same bubblewrap, same code path — the VM merely supplies the kernel.

**`LoudBestEffort`** — undeclared access is *intercepted and denied* by a policy layer (the generated Seatbelt profile on macOS) rather than removed from existence. Most violations fail loudly — genuinely valuable; it is the difference between Anneal-on-a-Mac and the lightweight tier. But the guarantee has known gaps (`docs/sandboxing.md` §3: metadata visibility, the Darwin runtime allowlist, profile coverage holes, a deprecated mechanism underneath). The claim weakens to *violations are probably caught*. Action success no longer proves input completeness — so nothing built here may be promoted beyond this machine.

A useful generalization: a Linux host without user-namespace support (hardened kernels, some containers) degrades to `LoudBestEffort` instead of failing or silently pretending — the grade machinery covers every "sandbox weaker than advertised" situation uniformly, not just macOS. (Implementation note, from the trust-plumbing landing: the code carries a third grade, **`Unenforced`**, because the no-sandbox-backend cfg fallback exists and "weaker than advertised" must include "absent." Today's Linux backend fails hard rather than degrading when bubblewrap is unusable, so `Enforced` is accurate whenever a sealed action actually runs there; the userns degradation above is future behavior, and lands as a grade change, not a new mechanism.)

**The grade appears nowhere in the rule API, deliberately.** Rule authors declare requirements and contracts (`ActionSpec`); the grade is a fact about the host, resolved at scheduling time. Three reasons: (1) a rule that branches on grade bakes the host into the action graph — analysis must remain a function of (attrs, configuration, deps, queries); (2) there is nothing sound an author could do with it — pillar 4 says authors restrict, never escalate, and `max_tier` already exists for restriction; (3) it keeps keying clean — **the grade never enters the action key**, because it isn't part of what the work *is*; it governs where the *result may be trusted*. Same action key on a Mac and in CI: same work, different provenance.

**The consumer/producer asymmetry, stated explicitly:** `LoudBestEffort` hosts are **full consumers of the shared cache but never producers into it.** A Mac laptop freely downloads CI's hermetic artifacts (their trustworthiness comes from the `Enforced` producer, not the consumer), keeps its own Local cache for everything it builds, and uploads nothing. Cache entries carry provenance — producing platform and grade — so this is auditable rather than folkloric, and "why didn't my colleague hit the cache" has a one-command answer.

**Surfaces:** `anneal query --explain-trust` shows the grade alongside attestations (its natural family): *"CargoBuild: built under LoudBestEffort (darwin-arm64, seatbelt) → tier Local, audit 5%. Promotable when built under Enforced (linux-vm, ci)."* A workspace/CI floor — `[trust] minimum_grade = "enforced"` (or `--require-enforced`) — makes the build **fail rather than silently degrade**; this is the mandatory CI posture, since CI quietly running Seatbelt would poison the invariant the consumer asymmetry rests on. And one honest sentence at first use (§1.5 style): on the first sealed action under Seatbelt, the CLI states the deal — best-effort isolation, results stay on this machine — at the moment it's relevant.

---

## 3. The rule API

The v3 sketch (`anneal_rule_api_v3.rs`) is the reference for shape; this section records the decisions and the round-3/round-4 amendments that supersede parts of it.

### 3.1 Rules declare; the framework executes

`Rule::analyze(ctx, attrs) -> Providers` is a pure declaration function: it registers actions and returns providers, never touching the filesystem. The API enforces this by construction — `AnalysisCtx` exposes no filesystem or process access, so the only effectful thing in the system is an action. Anything that must *run a tool* to be known goes through `ctx.query` (§3.6) and is itself a tracked action.

The analysis memo key is explicit: **(rule fingerprint, attrs, Configuration, dep provider digests, query output digests)**. Compiled-Rust rules don't hash their own source the way Starlark rules do; the fingerprint is supplied at registry construction — coarsest sound choice is the anneal binary's build id (every release invalidates all memos; acceptable), refinable to per-rule-crate versioning later, with `PROVIDER_NAME` (§3.8) as the recorded prerequisite of that refinement.

Analysis determinism is load-bearing for memo soundness, and Rust gives rule authors footguns Starlark doesn't (HashMap iteration order, time, ambient globals). Mitigations: `BTreeMap` discipline in the API, and the audit philosophy applied one level up — sampled re-analysis with action-digest comparison catches nondeterministic rules the way differential rebuilds catch unsound state.

### 3.2 Symbolic artifact identity

`OutputArtifact` is the analysis-time currency: minted by `declare_file`/`declare_tree` (the file/tree split is up-front because tree objects change CAS representation), bound to its producer by appearing in `ActionSpec::outputs`, consumed via `Input::Artifact`, flowing to dependents in providers. **Digests have no analysis-time representation at all**; they exist only post-execution and are resolved by the framework. This matches the codebase's existing `ArtifactSource::Output { action, name }` shape.

Identity is **content-derived — (target, configuration, path) — never a counter**. This is a memoization-correctness requirement before it is a persistence feature: re-analysis must mint identical identities for identical declarations regardless of declaration order, or any nondeterminism in ordering silently churns downstream keys. (The counter in v2 was the same sin as pub fields, one layer down.)

Output ownership: each artifact is owned by exactly one registered action per configured target; zero or two is a hard analysis error. Configuration (§4) is what reconciles this invariant with dev/hermetic variants.

### 3.3 Sealed capability types

Handles (`OutputArtifact`, `StateHandle`, `ToolTree`, `ProgramRef`) have private fields and are mintable only through `AnalysisCtx` or validating constructors (`RelPath::new` rejects absolute paths and `..`). The sketch proves this with module structure — examples live in a sibling module and compile against the public surface only — and the real crates should preserve the property. The chain it buys: `StateUse::Mutate` is unconstructable without a `StateHandle`, which is unmintable without `declare_state`, which won't accept `Interleaved` without an `Attestation`. The compiler checks the loudest invariant in the design.

What remains runtime-checked, inherently (recorded so nobody mistakes docs for types): `declare_state` idempotence (bit-identical declarations dedup across targets; any mismatch is a hard error; identical `Produce` actions dedup by action key), output ownership, and the Hermetic-Mutate rejection (§4.4).

### 3.4 Materialization and the mtime trap

The CAS uses hardlink materialization, which interacts with mtime-based tool freshness (cargo) in one specific dangerous way: a file *reverting* to content already in CAS resurrects an old inode whose mtime is *older* than the tool's fingerprint, so the tool may judge it clean — a silently-stale output delivered by two individually-correct systems. The answer is `MutateMtimes::BumpOnContentChange` on the `Mutate` grant: any input whose content differs from what that state's per-`StateKey` materialization journal last observed — including reverts — gets a fresh mtime. The policy lives **on the grant** (round-2 relocation) because the journal is per-key and the policy is meaningless without a referent state; per-action policy is reduced to `Epoch` (required for `ByteDeterministic`) and `Preserve`. `MutateMtimes::ActionPolicy` exists for content-keyed, mtime-blind tool state (GOCACHE). Cargo's unstable `-Zchecksum-freshness` is the upstream acknowledgment of this problem class and a candidate alternative.

### 3.5 Programs imply their input edges

`ProgramRef` is minted (tool-tree-rooted via `ToolTree::program`, or artifact-rooted via `ProgramRef::from_artifact` — the latter is how graph-built codegen tools run). **Both variants imply their input edge automatically**: an artifact-rooted program is a graph edge, and an unimplied edge is a wrong graph, not a runtime failure, so the framework derives it — forgetting is impossible rather than detectable. This decision composes with the coloring theorem to produce an emergent corollary (§4.3).

### 3.6 Queries: the round-3 amendment, amended again in round 4

The recurring bug of all three sketch rounds was the query mechanism (v1: unusable `ActionHandle`; v2: unmintable `QueryRef`; v3: `spec()` can't mint outputs and `ActionSpec` has no stdout — no `ToolQuery` was implementable). The resolution:

**`QuerySpec`, a narrower type than `ActionSpec`:** inputs restricted to `Sources` / `Tool` / `Unmanaged` **plus `Read` of phase-separated state** — this last breaks from the round-3 reviewer's `state: none` sketch, which breaks real queries: cargo metadata with dependency resolution reads the registry, the Nickel import closure resolves through fetched packages, `go list` reads GOMODCACHE. Phase-separated reads are sound here because they key on producer action keys. Output is **implicit: stdout, captured and parsed as `Q::Output`**. `ByteDeterministic` and `NetworkPolicy::Denied` hold **by construction, not convention** — pillar 3 applied to the framework's own mechanism. No interleaved state, no declared artifacts.

**The bootstrap split (round-4 amendment — the cycle fix).** Round 4 caught v1 of this document shipping a demand cycle as two separately-correct decisions three sections apart: the metadata query reads the registry (this section), and CargoFetch's include set derives from the metadata member list (§8) — so metadata → registry → CargoFetch → metadata. The resolution is **two queries with different capability needs**: a **bootstrap query** (`cargo metadata --no-deps`) that reads only workspace manifests — registry-free — and feeds CargoFetch's include set; and the **full resolution query**, which runs after the registry exists and declares `Read` of it. The general principle, recorded so the next ecosystem doesn't rediscover it: *state-reading queries induce a bootstrap ladder, and the ladder must ground out in a query with no state dependencies.* A rule that suspends on a state-reading query must have declared the state and registered its producer first, and whatever the producer itself needs to know must come from a state-free query.

**Registration finality (round-4 amendment).** For a query to read state produced by an action registered moments earlier in the same `analyze`, actions registered before a suspension point must be **demandable while the registering analysis is suspended**. If the analysis subsequently fails, its registrations are retracted from the graph (nothing further demands them), but executions already completed stay cached — they were keyed and sandboxed like any action, so their results are sound regardless of the fate of the analysis that requested them. The **demand-cycle checker covers state edges** (query → state → producer action → registering analysis): this is the edge class the round-4 cycle traveled, and artifact-edge checking alone would not have caught it.

**`Input::Artifact` is excluded from `QuerySpec` as a named decision.** Admitting it means analysis can suspend on arbitrary builds — full Buck2 dynamic-dependency territory. The suspension architecture supports it (and the earlier conversation's claim that a generated `.ncl` feeding another target's import query is "legal and natural" conflated what the architecture *permits* with what the API should *expose*); it is deferred, deliberately, and should be revisited as a v-later capability with its own design pass.

**`Enumerated { files: Vec<RelPath> }`, and `QueryRef` is deleted.** The refinement ladder's second rung carries the projected file list as data: the rule extracts paths from the query's parsed output itself, keeping the framework out of the introspection business. The reference the reviewer kept "for invalidation" is redundant — the analysis memo already keys on query output digests, so a changed enumeration re-runs analysis and regenerates the list. Soundness end-to-end: the query's own inputs span the unmanaged root coarsely; the expensive action gets the narrow list; a stale or fabricated list cannot cause silent staleness because the sandbox makes omissions loud (missing file ⇒ loud failure) and additions merely over-key. `QueryRef` joins `ActionHandle` as the second deleted vestigial handle.

**The memoization contract sentence, preserved verbatim because it's the keystone:** a query's *input* granularity becomes the analysis *invalidation* granularity — and because queries are `ByteDeterministic`, an input change that leaves the query output byte-identical cuts off **at the analysis boundary**: no re-analysis, no action key changes, cascade dead. Early cutoff on query actions is what makes the suspension model fast rather than merely sound. (This determinism has an implementation dependency surfaced in round 4: tools emit absolute paths — `cargo metadata`'s JSON is full of them — so query byte-determinism requires a **constant sandbox root** across executions. Per-action random temp roots would silently break the keystone. Spike item, §10.)

One subtle constraint from the round-3 review, worth a doc sentence in the crates: query specs dedup across configurations by action key (the metadata query is identical under Dev and Hermetic, so both configured analyses share one execution) — which means a query spec that accidentally folds configuration in (e.g. profile in env) silently doubles execution.

### 3.7 The refinement ladder (partial adoption)

An unmanaged dependency starts as `UnmanagedGranularity::OpaqueTree` (whole-subtree hash: coarse, always sound — the floor). The first refinement uses ecosystem metadata to enumerate the file set without wrapping the dep (`Enumerated`, §3.6). Full wrapping is the top rung. Every rung is conservative and explicit in the graph (`anneal query` shows where the coarse edges are); granularity is a performance upgrade, never a correctness gamble. Source sets are deliberately **not gitignore-derived** — ignored files (`.env`, local configs) routinely affect builds; the input set is stated, and the sandbox makes the statement honest since only materialized paths exist inside it. `Unmanaged.root` should become a validated `UnmanagedRoot` type clarifying repo-relative vs. absolute (round-3 smaller item).

### 3.8 Providers

Typed inter-rule data, Buck2-flavored. The working serialization shape (round-3 fix; v2's was uncompilable): storage is `Arc<dyn ProviderObj>` where the object trait carries `as_any()` (downcast), `erased()` (the `erased_serde::Serialize` vtable, legal via its blanket impl over `serde::Serialize`), and `name()`. `Provider: Any + Send + Sync + serde::Serialize` plus `const PROVIDER_NAME: &'static str` — versioned names (`"cargo.artifacts.v1"`). The dependency arrow, recorded: name-keyed deserialization (typetag-shaped — take typetag off the shelf) is *masked-optional* today, because binary-fingerprint nuking means persisted memos are only read by the binary that wrote them, where `TypeId` is stable — and becomes *load-bearing* exactly when rule fingerprints refine to per-crate. `Provider: Serialize` is a day-one constraint because persistence rung two (§6.2) stays open only if it is.

`DepHandle` exposes `config()` — deps resolve as actually configured by the coloring policy (§4.2), and artifacts carry their configuration individually via `OutputArtifact::config()`.

### 3.9 Spec/policy split

`ActionSpec` is identity: **every field keys the action**. `ExecPolicy` is observation: **no field keys** (audit, `max_tier`, future timeout/priority/locality). The split makes the keying question unaskable rather than documented — chosen over a doc table because tables rot the day someone adds a field. (The enforcement grade, §2.8, follows the same logic from the platform side: it never keys, because it is provenance, not identity.) `Rule` is non-object-safe (`type Attrs`, `const NAME`); the blanket-implemented `ErasedRule` adapter is the registry's storage type, budgeted rather than discovered at integration.

---

## 4. Configuration: coloring, not transitions

### 4.1 The shape

A configured target is `(target, Configuration)`; configuration is folded into analysis memos, action keys, and artifact identity. The axis set (amended per Appendix A, ruling 3): the **existing implemented build axes** — platform plus `OptLevel`/`Lto`/`DebugInfo`/`Sanitizer`/`Coverage` in `anneal-core` — **plus `ExecMode { Incremental, Hermetic }`**, the one axis the cone requires. The sketches' `BuildProfile { Dev, Release }` was a coarse stand-in for the implemented axes and is superseded by them. All axes get **per-rule interpretation** (each rule decides what `Hermetic` means for its tool), and the code's per-action **consumed-axes key trimming** (an action's key includes only the axes it consumes) is kept — it's an early-cutoff mechanism this doc failed to describe and the code got right. Deliberately absent: user-defined transitions, `select()`. Bazel's configuration complexity radiates overwhelmingly from transitions making configuration *a function of the path you arrived by*; Anneal's restriction keeps it *a function of the node*, and that single property is the firewall.

### 4.2 The focus cone

Per-invocation **graph coloring**, assigned by framework policy before analysis from VCS dirty-state and edit horizon: the Incremental cone is the edited targets **plus their transitive dependents** (anything downstream of an edit rebuilds anyway and benefits from warm state); everything upstream is Hermetic, where unchanged inputs make it a pure shared-cache hit. One configuration per node per invocation; no target analyzed twice in one build; nothing user-composable. This is also the answer to the cross-config cache-poisoning trap: without the cone, every action downstream of a dev-built artifact misses the shared cache forever; with it, the warm mutable inner loop covers the crate under your fingers while CI-built hermetic artifacts serve the other 9,900 targets.

`ctx.dep` returns the dep's providers *as actually configured* — a dev-mode target routinely consumes hermetic-mode deps, and that asymmetry is the cone working as intended.

Consequences recorded: **entering the cone is an invalidation event distinct from touching a file** — a node flipping Hermetic→Incremental changes its configured identity, hence its memo and every artifact identity it mints. The cascade is bounded by byte divergence at action-key resolution, and assignment must be **sticky** (dirty-state with hysteresis, never per-keystroke) or the flip cost is paid as thrash.

Coloring honesty (round-4 addition): the file→target mapping that seeds the cone comes from prior invocations' source sets and load-time approximation, so **mis-coloring is possible** (a brand-new file, a changed glob). Mis-coloring is strictly a performance bug — a Hermetic node with dirty inputs still builds its actual content, keyed by content — never a soundness bug. Recorded so nobody "fixes" it with a soundness-flavored hack.

### 4.3 The monotonicity theorem

**No Hermetic node ever depends on an Incremental node.** Proof: if Hermetic X depended on Incremental Y, X would be a transitive dependent of an edit, hence Incremental — contradiction. The entire promotion story rests on this invariant, so it is not merely documented but **asserted by the framework at edge-resolution time**, because a future coloring-policy tweak could silently break it and a Hermetic node consuming dev-built bytes is a poisoned shared cache.

Two additions beyond the round-3 review. The assert *constrains the pin feature*: any future "pin this target Incremental" debug flag must take the monotone closure (pinning X flips X's dependents too) or be rejected — otherwise the flag is a poisoned-cache generator with a friendly name. And the corollary: because programs imply input edges (§3.5), **no Hermetic action can ever execute a dev-built tool** — the theorem covers program edges automatically. This property *emerged* from two independent decisions composing; it was not designed, which is exactly why it's written down.

### 4.4 Hermetic is enforced

`register` hard-rejects `StateUse::Mutate` under `ExecMode::Hermetic` — the one-line check that makes Hermetic a guarantee rather than a naming custom. Precision so nobody over-rotates: `Allowlist` network **remains legal under Hermetic** (fetch actions must run in CI to populate phase-separated state); it caps at Local as always. Hermetic means "no interleaved mutation," not "no network."

### 4.5 The exec-configuration deferral, named once

`ToolchainKey.platform: "host"` and "built tools run in the configuration that built them" (`ProgramRef::from_artifact`) are **the same deferral**: the exec-configuration question (host-vs-target, in Bazel vocabulary). When the platform axis joins `Configuration`, both land together. The execution-platform routing in §7 is the first concrete pressure on this deferral and the shape its resolution must fit: per-action platforms with enforcement grades are the *execution* half of the question; target-platform configuration is the half still deferred.

---

## 5. Graph architecture

### 5.1 Three layers, separated by kind rather than time

Loading evaluates Starlark into the *target graph* (configured-target nodes: rule + attrs + dep edges, pure data). Analysis maps each node through `analyze` into its *action subgraph* plus providers; the action graph is bipartite (actions ↔ artifacts) and clusters compose exclusively through artifact edges. Execution runs demanded actions.

**Strict temporal phasing (Bazel's model) is impossible for Anneal on purpose.** Bazel's all-actions-before-any-execution promise leaks precisely at the native-tool boundary — repository rules (historically unsandboxed pre-phase) and discovered inputs (`.d` files) are its two escape hatches — and Anneal's entire thesis lives at that boundary. `cargo metadata`, import closures, `go list` are dynamic dependencies by nature; jamming them into an unsandboxed pre-phase would betray enforcement exactly where it matters most. Queries-as-sandboxed-actions makes pure phasing unachievable *by design*.

The replacement is Buck2's move: **one incremental, memoized computation graph** where loading, analysis, and execution are node *types* with different purity contracts. The clean separation survives as an API invariant (analysis has no effects; the type system enforces it) rather than a phase barrier. The graph unfolds demand-driven — analysis suspends on queries, queries demand artifacts and phase-separated state, state demands its producer, producers may be registered by the suspended analysis itself (registration finality, §3.6) — a DAG of suspensions, well-founded so long as demand edges stay acyclic. **The cycle checker covers all demand-edge classes — artifact edges *and* state edges** (query → state → producer → registering analysis); round 4 demonstrated that the state-edge class is where real cycles hide.

Engineering choices: `analyze` is effectively async (suspension wants it; the rejected alternative is Skyframe-style restarts — synchronous API, wasted recomputation, side-effect prohibitions). **salsa** is the candidate off-the-shelf memoization engine (rust-analyzer's foundation; its red-green invalidation is exactly the early-cutoff semantics required). Warm, the full graph materializes as an in-memory walk over cached queries, so `anneal query` is fast; cold, building the graph genuinely runs the metadata queries — honest, cheap, and what cargo users already live with.

### 5.2 Scale calibration

Tens of thousands of targets is *small* — Bazel and Buck2 hold millions of action nodes in memory. A 10k-target repo expands to ~50–200k actions at a few KB each: low hundreds of MB. The design question is never capacity; it is **avoiding recomputation across invocations** — latency. Demand-driven laziness (Buck2's full laziness, not Bazel's eager loading) is what matters if six figures ever arrives.

---

## 6. Persistence, daemonlessness, and CI

### 6.1 Durable stores

The live graph does **not** live in a database; it lives in the memo layer. Durable state is three boring stores plus a journal: the **action cache** (action key → output digests + metadata, including producing-platform provenance per §2.8; pure KV), the **CAS** (filesystem tree, hardlink-materialized), the **file-digest cache** mapping (path, size, mtime, inode) → content digest — literally git's index design, stat-validated content addressing — and the **materialization journal** (already required by `BumpOnContentChange`). SQLite in WAL mode or redb; Buck2 keeps materializer state in SQLite, so there's precedent. The graph's *structure* is cheap to rebuild; only *results* are expensive.

### 6.2 The analysis-persistence ladder

Anneal gets, nearly free, what Bazel and Buck2 famously lack (memory-only analysis graphs; "analysis cache discarded"): because the expensive part of analysis is the queries, **and queries are actions**, they hit the persistent action cache. Cold-start is "read action cache, re-run cheap pure functions" — plausibly seconds at 10k targets. The ladder: rung 0, persist nothing (Starlark re-evaluation dominates, ~0.5–2s warm); rung 1, snapshot the post-Starlark target graph keyed per-package on BUILD-file digests; rung 2, persist analysis memos (requires `Provider: Serialize` — imposed day one). None of this is the fragile serialize-Skyframe problem (that's millions of nodes plus Java object graphs). Binary-version skew is solved by the fingerprint decision: mismatch nukes the memo store, one cold rebuild per upgrade, correct by construction.

### 6.3 Git: accelerator and label, never key

Commits cannot be the identity layer — developers build from dirty trees nearly always, so commit-keyed caching misses on every warm path. Content digests are the truth. Git's roles: **change-detection accelerator** (diff/status or an external watcher narrows the stat-walk) and **labeling scheme** — per-commit *fingerprint manifests* (target → transitive digest, a few MB) make `anneal affected base..head` a map comparison with no checkout or re-analysis of base. This is the proven bazel-diff pattern, nearly free here because every target already has a transitive content key. The instinct "diff the stored graph against the current tree" is satisfied by memoization itself: correct memo keys plus early cutoff *are* the diff algorithm — incremental recomputation is derived from keys, never implemented as an explicit diff engine. Manifests double as **GC roots**: recent commits' manifests plus the live session are roots; unreachable-past-threshold is collectable — the pinning feature *is* the retention policy.

### 6.4 Daemonless, with the no-authority principle

Decision: **build daemonless; keep the latency budget honest at ninja-class** (a few hundred ms warm no-op via parallel stat-walk against the digest cache — ninja proves this suffices at Chromium scale). Precision added in round 4: **the ninja-class number presupposes persistence rung ≥ 1** (§6.2). At rung 0 the warm floor is Starlark re-evaluation, ~0.5–2s — the latency commitment is tied to the rung shipped, so the first benchmark can't embarrass the doc.

The daemon's four benefits decompose: watching (optional, consumable via external watchman, git-fsmonitor-style); memo retention (the persistence ladder); concurrency (no global lock — CAS is lock-free idempotent via write-temp-rename, SQLite WAL handles the KV stores, per-`StateKey` advisory flocks with PID+boot-id staleness handle `Exclusive` state, and same-action demand-merging is an flock on the output slot: loser blocks, then cache-hits the winner's result); background work (spawn-and-detach maintenance, git's auto-gc pattern — **GC must never depend on a resident process**, or CI runners and one-shot agent sandboxes break).

`--watch` is the interactive answer: a *foreground* process holding the memo graph and subscribing to file events — every daemon benefit, opt-in, visible, dying on Ctrl-C. Mechanism honesty: it cannot share memory with other invocations; it keeps **disk truth maximally fresh** by eager evaluation, so sibling commands degenerate into cache reads. The warmth is scoped to its demanded cone, and the floor for siblings is still the daemonless floor. IDE-grade latency, if ever wanted, is `--watch --serve`.

The governing principle, which is the brand argument as architecture: **a daemon may hold no authority.** In Bazel/Buck2, daemon memory is partially truth — "restart the daemon" fixes things, which is the cache-distrust disease one level up. Every durable fact lives on disk in content-keyed stores; any future daemon is a transparent accelerator that structurally cannot lie. Daemonless-and-fast is the proof the state model is honest. (The macOS Linux VM is a resident process and is held to the same principle — §7.4.)

---

## 7. Execution platforms

New in v2. The trust-model half of this story is §2.8 (the grades, the tier row, the consumer/producer asymmetry); this section is the mechanism.

### 7.1 The platform inventory

An execution platform is where a `Sealed` action runs, carrying an enforcement grade as a typed property:

| Platform | Mechanism | Grade |
|---|---|---|
| linux-native | bubblewrap namespaces | `Enforced` |
| linux-vm (on macOS) | same bubblewrap, inside an anneal-managed VM | `Enforced` |
| darwin-native | Seatbelt profile (`sandbox-exec`) | `LoudBestEffort` |
| linux-native, no userns | bubblewrap degraded | `LoudBestEffort` |

Routing is per-action and automatic: darwin-target actions (Mac test binaries, dylibs, codesigning — anything that must execute Darwin) run darwin-native; linux-target actions run in the VM when present, else darwin-native with the Local cap. `anneal build` on a Mac just works; the grades govern trust, not availability.

### 7.2 macOS native: Seatbelt, graded honestly

Already implemented (`anneal-exec/src/sandbox.rs`): sealed actions run under generated deny-by-default Seatbelt profiles (network denied, undeclared reads/writes denied, environment scrubbed), with `clonefile` CoW materialization so a misbehaving action cannot corrupt the CAS, and read-tracking as the diagnostic layer (`docs/sandboxing.md` §§2–4). This is materially stronger than anything in the lightweight tier and it is graded `LoudBestEffort`, not `Enforced`, for the reasons recorded there: policy interception rather than structural absence, metadata visibility, a Darwin runtime allowlist, and a deprecated mechanism underneath (`sandbox-exec` is deprecated with no published CLI-sandboxing successor; Bazel and Nix remain on it; the subsystem underlies Apple's own sandboxing and is not going anywhere soon — but the API risk is real and the VM is the hedge).

Darwin-native execution is mandatory regardless of any VM: **a Linux VM cannot produce or run Darwin artifacts.** The VM is not a replacement for Seatbelt; the two are complements routed by target platform.

### 7.3 The Linux VM: CAS-inside, vsock, no virtiofs on the hot path

The reason Docker-on-Mac builds feel slow is virtiofs: bind-mounting the host repo puts a slow, metadata-chatty filesystem boundary on the hot path of a metadata-heavy workload. **Anneal never needs to do that**, because actions don't run "in the repo" — they run in sandboxes materialized from a CAS. The architecture:

- The **CAS, action cache, warm interleaved state (cargo `target/`), and all action execution live inside the VM** on its native ext4 disk. No virtiofs on any hot path.
- The only host↔VM traffic is **content-addressed and O(change)**: source ingestion (the host stat-walk against the file-digest cache runs at native APFS speed; only changed blobs stream over vsock) and demanded outputs coming back (worktree sync, artifacts the user asked for).

The inner loop becomes: edit one file → host digests it natively → one blob crosses vsock → warm cargo runs entirely VM-native, where Virtualization.framework CPU overhead is low single digits. **The VM tax is paid per byte changed, not per file in the tree** — and bytes changed is exactly the user's edit. This promise is a consequence of the CAS architecture; a virtiofs-mounted design could not make it.

Substrate candidates, in order of how much we'd own: Apple's open-source containerization stack (`apple/container` / `apple/containerization`: per-container lightweight VMs, sub-second boot, vsock gRPC init — proof the primitive is cheap; its virtiofs file-sharing we'd bypass anyway); direct Virtualization.framework or libkrun (one persistent anneal-owned image running the exact same bubblewrap sandbox inside — the key correctness property: per-action isolation is the same code path as native Linux, so the grade is `Enforced` with no new trust analysis); bring-your-own Linux (colima, a remote box — the VM is just "an execution platform running anneal-exec," right shape for the spike, wrong shape for the product because an external dependency is a first-afternoon tax).

**Architecture nuance worth designing around:** an arm64 VM builds arm64-linux natively. Enabling Rosetta for Linux VMs lets the same Mac produce **byte-identical x86_64-linux artifacts at roughly 20–30% penalty** (vs. 2–5× for QEMU emulation) — meaning a laptop can *produce and consume the same Promotable cache entries as x86_64 CI*. That is a headline feature, not a footnote: laptop↔CI cache convergence, which no graveyard project had. Caveat recorded: enabling Rosetta forces TSO memory ordering VM-wide, taxing arm64-native code in the same VM — so the design likely wants **two VM flavors keyed by target arch** rather than one VM with Rosetta always on.

### 7.4 The VM holds no authority

The VM is a resident process, and it is held to §6.4's principle with one sentence: **the VM may hold no authority.** All durable truth lives on its virtual disk (CAS, action cache, journal — content-keyed stores, same as the host's); the VM process is a transparent accelerator, killable at any moment, re-bootable in under a second (Apple's container stack demonstrates the boot cost). Linux CI runs no VM; the VM exists only on Mac hosts.

### 7.5 Sequencing

**Not in Milestone 1.** Linux-native and darwin-Seatbelt both exist today, and the demo gate (warm cargo inner loop) is provable on both. The VM is the first post-M1 spike, with BYO-Linux as the cheapest spike harness and two genuine unknowns to measure: vsock ingestion throughput for large one-time blobs (toolchains), and VM-side warm-state lifecycle across VM restarts.

---

## 8. Per-ecosystem notes

**cargo (hard mode — interleaved).** Sound configuration cone: `--locked`, pinned toolchain, `CARGO_INCREMENTAL=0` + `--remap-path-prefix` for the Hermetic/promotable variant. `target/` is `Interleaved { Exclusive }` (cargo's lock made graph-visible), sharded per (workspace, profile) so exclusivity costs parallelism only within a shard. `BumpOnContentChange` is mandatory (the mtime/hardlink revert trap, §3.4). Registry is phase-separated, produced by `cargo fetch --locked` keyed on the lockfile. **Two queries, per the bootstrap split (§3.6):** the registry-free bootstrap query (`cargo metadata --no-deps`, workspace manifests only) feeds CargoFetch's include set — hand-written `Cargo.toml + Cargo.lock` misses workspace member manifests — and the full resolution query runs after the registry exists, declaring `Read` of it. The bootstrap fix self-referentially demonstrates query-derived input sets, which is the point of queries. Soundness history (1.52.0 incremental miscompilation) is the epoch mechanism's reason to exist. sccache requiring incremental-off independently corroborates the dev/hermetic split.

**pnpm (easy mode — phase-separated).** `pnpm install --frozen-lockfile --ignore-scripts` produces node_modules keyed on the lockfile's integrity pins; consumers get read-only mounts. Lifecycle scripts become explicit follow-up actions so native postinstalls can't smuggle undeclared fetches. No attestation anywhere — the shape to prefer.

**Go (best case).** GOCACHE is interleaved but content-addressed and concurrency-safe (`SharedSafe`); toolchain reproducible by upstream policy (`-trimpath`, reproducible toolchains since 1.21); GOMODCACHE phase-separated with go.sum integrity. Near-free (a) and (b). `go list` is a state-reading query (GOMODCACHE) and slots into the §3.6 bootstrap ladder the same way cargo does.

**Nickel (the trivially-hermetic calibration point).** Pure evaluation: no state, no attestation, empty capability surface — *ceremony should correlate with risk*, and Nickel validates the API's gradient. `ByteDeterministic` exports make early cutoff the routing payoff: a comment edit re-exports identical bytes and the downstream cascade dies, which is what retires regenerate-and-diff CI tripwires. Imports are syntactically static, so the closure query is a parser (chicken-and-egg resolved as in C++ include scanning: cheap query keyed coarse, expensive exports keyed narrow). Package fetch mirrors CargoFetch (`Nickel-pkg.ncl` manifest + lock, GitHub-hosted index). **Worktree sync** (exports materialized into the repo for unwrapped consumers — IDEs, unmanaged tools) is deliberately *not an action*: it's a registration `anneal sync` honors, hardlinked from CAS with a journal so staleness is detected. It's the feature that makes partial adoption pleasant rather than merely sound; conflict semantics are open (§9).

**Gradle (the falsifier).** Daemons holding cross-action state, mutable `~/.gradle`, nondeterministic annotation processors, jar timestamps: the sound configuration cone is so narrow and so contrary to Gradle's nature that the honest rule is wrap coarsely, `max_tier`-restrict aggressively, audit heavily. The model degrades gracefully there rather than breaking — and the docs should present the per-ecosystem burden gradient (Go trivial → Gradle adversarial) rather than implying all wraps are equal.

**TypeScript / Python, brief.** tsc moderate (`.tsbuildinfo` decent, hazards are the bundlers around it); Python nearly pnpm-shaped if constrained to uv + wheels-only (hash-pinned locks, content-addressed wheel cache, phase-separated venvs) — sdist native builds reintroduce the swamp.

---

## 9. Open questions

**Spike-blocking (see §10):** stdout capture mechanics in the sandbox (QuerySpec's implicit output — what bubblewrap/Seatbelt actually have to do); **sandbox path stability** (query byte-determinism requires a constant sandbox root across executions, §3.6 — if `anneal-exec` uses per-action temp roots today, the keystone early-cutoff property silently breaks); materialization-journal implementation (storage, crash consistency, interaction with hardlink promotion).

**Named deferrals:** exec-configuration (platform axis + built-tool configuration, §4.5 — now with §7's execution-platform routing as the first concrete constraint on its shape); dynamic dependencies (`Input::Artifact` in `QuerySpec`, §3.6); **test-result caching** (named in §1's value chain, designed nowhere yet: flakiness policy, what a cached "pass" means under `ReuseSound`, retry semantics — the costume the lightweight tier most visibly fails at, so it needs a real design pass before the chain argument is honest); worktree-sync conflict semantics (hand-edited worktree copies, eager-vs-`anneal sync` materialization); cone-assignment policy details (hysteresis parameters, the pin flag's monotone-closure requirement, §4.3); GC thresholds and manifest-retention policy (§6.3); the Linux VM spike unknowns (vsock ingestion throughput, VM-side warm-state lifecycle, §7.5); remote execution (explicitly v1.x, out of scope for the Thesis MVP).

---

## 10. Handoff to the implementation session

**Spike first, refactor second.** Before any API refactoring, wire a `QuerySpec`-shaped `CargoMetadataQuery` into the existing `anneal-exec` action model end-to-end — **both queries of the bootstrap split** (§3.6), since the split is where round 4 found the cycle. The predicted pressure points — stdout capture, sandbox path stability, and the materialization journal — live in the sandbox's contact with reality, where no sketch review can find what's wrong. If any forces a reshaping, it must be known before the v3 surface propagates through the crates. The spike's findings amend this doc; this doc arbitrates when spike, sketch, and code disagree.

### Spike report (June 11, 2026 — `anneal-exec/src/query.rs`, `tests/query_spike.rs`)

The spike landed: both bootstrap-split queries run end-to-end through the real sealed sandbox on macOS (Seatbelt) and under the Nix dev shell, with caching, and **no reshaping of the design was forced**. Findings, in decreasing order of surprise:

1. **Two unplanned sandbox-surface findings, both fail-closed.** (a) Apple's LibreSSL reads `/private/etc/ssl/openssl.cnf` at library init and **ignores `OPENSSL_CONF`**, so anything linking system libcurl (rustup-distributed cargo, git) aborts under the deny-by-default profile. Resolution: `/private/etc/ssl` joined the Darwin runtime allowlist (same near-constant class as `/private/var/select`); `SANDBOX_VERSION` bumped 6→7. Violation-vignette candidate (§1.5). (b) Nix-store toolchains dylib-link **across store paths** (cargo → sibling libiconv), so a bin-parent-only toolchain mount fails closed. The manifest's `read_only_roots` closure is the already-correct mechanism — and a constraint on Appendix A ruling 1: provisioned toolchains must carry their **closure**, not just a bin dir.
2. **Sandbox-root stability is load-bearing exactly where predicted, with one precision gained:** the query root is keyed by query *identity* (command/env/toolchains/working-dir — input digests excluded), and the keystone test passes — a manifest comment edit changes the key, the query re-runs, and stdout is byte-identical, so cutoff survives. The precision: on **Linux this is belt-and-suspenders** (bwrap binds the root at the fixed guest path `/work`, so emitted paths are machine-independent regardless of host root); on **macOS the host path leaks into the output**, so the stable root is what delivers per-checkout determinism — and cross-machine query-byte convergence is structurally Linux-only, consistent with §2.8's consumer asymmetry.
3. **stdout capture needed real plumbing, not a flag:** pipes (the sandbox nulls stdio by default) plus per-pipe drain threads, because `cargo metadata` exceeds a pipe buffer and a synchronous wait deadlocks. stderr rides along into the `QueryFailed` diagnostic — which earned its keep immediately (both findings in (1) were diagnosed from it). Cache entries are namespaced (`anneal-query-v1`), stdout blobs live in the CAS.
4. **Builder-narrowing was sufficient — no separate `QuerySpec` data type needed.** The spike's `QuerySpec` wraps a constrained `ActionBuilder` exposing only inputs/env/toolchains/working-dir/timeout; Sealed + Deterministic + network-denied + output-less hold because no method can change them. Existing action validation composed for free (it immediately rejected a PATH entry outside toolchain roots). Pillar 3 by type, at the framework's own mechanism, as §3.6 specified.
5. **Not yet exercised, deliberately:** phase-separated `Read` in queries (registry-backed full resolution) waits for the state-taxonomy work — the full-resolution query ran over path-deps, which confirms the bootstrap rung needs no state but leaves the `Read` rung untested. The materialization journal was untouched (queries wipe and re-materialize; they have no mutable state by construction) — it remains the §9 spike-blocking item it was. Suspension/analysis integration is the next contact point.

**The change list, with round-3 and round-4 amendments resolved** (implement these as decided here, not as the unamended reviews proposed):

`Read(Interleaved)` is forbidden outright — no generation-counter fallback (§2.5). `QuerySpec` replaces `ToolQuery::spec() -> ActionSpec`, with stdout-implicit output, `ByteDeterministic` + `Denied` by construction, and inputs of Sources/Tool/Unmanaged **plus phase-separated `Read`** (§3.6). **The bootstrap split:** state-reading queries require their state's producer registered first, grounded in a state-free bootstrap query; registration finality as specified; state edges in the demand-cycle checker (§3.6, §5.1). `Enumerated` carries `files: Vec<RelPath>` projected by the rule; **`QueryRef` is deleted** (§3.6). `Input::Artifact` stays out of `QuerySpec` as a named deferral. The monotonicity assert lands at edge-resolution time, and the pin flag, when built, takes the monotone closure (§4.3). **Enforcement grades:** the tier computation takes (spec, platform); `enforcement < Enforced ⇒ at most Local`; cache entries carry producing-platform provenance; `LoudBestEffort` hosts consume but never produce the shared cache; `[trust] minimum_grade` floor for CI; grade surfaced in `--explain-trust` (§2.8, §7). `OutputArtifact` derives `PartialEq + Eq + Hash`. CargoFetch's include set derives from the **bootstrap** query's member list (§8). The `PathBuf` re-export goes; `UnmanagedRoot` becomes a validated type. One doc sentence on cross-config query dedup and its silent-doubling constraint (§3.6). The violation-vignette corpus is a deliverable of the diagnostics work, not an afterthought (§1.5).

**Process checklist, earned over four review rounds** (the recurrence pattern was: each round shipped one described-but-unconstructible mechanism — `ActionHandle`, `QueryRef`, stdout, and in round 4 the bootstrap cycle):

Every consumed type needs a producing example, compiled in CI (doctest-grade, so the fifth vestigial handle is impossible rather than catchable). Every "by convention" needs either a type or an assert. Every new concept ships with its mechanism in the API the same round it's introduced — the cone taught this one. **And the checklist applies to amendments too**: round 4's cycle was introduced by the amendment that corrected the round-3 reviewer, shipped without working out its producer ordering. Corrections get the same scrutiny as proposals, or the pattern just moves into the diffs.

---

## Appendix A: Code/design reconciliation (audited June 11, 2026)

A three-way audit of the codebase against this document found the expected maturity gaps (the v3 rule API, tiers/grades/provenance, the cone, queries, memoization, GC, and most of the §1.5 CLI surface are design-only) — but also **five places where the code and this doc embody different mechanisms**, each defensible. Per this doc's own arbiter role, those get rulings, not silence. Recorded here so neither side is "wrong by omission" again.

**What the audit confirmed is real and matches the design's intent:** the sandbox layer (bubblewrap / Seatbelt with environment hermeticity), the CAS with clonefile/hardlink materialization, the stat-validated file-digest cache (§6.1's git-index design, implemented), the content-keyed action cache with careful key composition, warm sandbox reuse with the input-manifest journal and crash-safe commit protocol (§2.2's bet, working), the cold-vs-warm verification harness (§2.7's embryo), and the `build`/`test`/`affected`/`why` CLI. **Both §10 spike risks were confirmed live:** sandbox roots are per-action random (`<key16>-<pid>-<nonce>`) and action stdout is nulled — exactly the two things `QuerySpec` needs changed.

### The five rulings

**1. Toolchains — both, phased.** Code: Nix-manifest host toolchains (`ANNEAL_TOOLCHAIN_MANIFEST` → `/nix/store/...` paths, identity = store path, no fetching). Doc: anneal-provisioned, digest-verified tool trees. Ruling: toolchains become **pluggable providers**. The Nix manifest stays (sound, tested, right for Nix-native teams); anneal-provisioned acquisition gets built as the **default adopter path**, because §1.5's "single static binary, no runtime deps" gate cannot survive a Nix prerequisite. `docs/why-anneal.md`'s provisioning claims describe the adopter path.

**2. Cargo dependency acquisition — FOD absorbed.** Code: per-dependency fixed-output fetch actions (`CachePolicy::FixedOutput`, output digest mechanically verified against lockfile checksums) plus in-sandbox vendor assembly. Doc: a `CargoFetch` action producing phase-separated registry state, with the §3.6 bootstrap split. Ruling: **FOD is blessed, not migrated away from** — it is precisely the "sandbox-recorded digests checked mechanically" mechanism §2.4 named as the sound replacement for the deleted network-promotion exception, and it already works. Phase-separated state remains the design for registry-*shaped* trees (pnpm's store, GOMODCACHE); the two coexist, and the §3.6 bootstrap ladder applies wherever a query needs produced state, regardless of which acquisition mechanism produced it. §2.4 amended in place.

**3. Configuration axes — code wins, plus ExecMode.** Code: platform + five build axes with per-action consumed-axes key trimming. Doc (pre-amendment): "two built-in axes." Ruling: **keep the implemented axes and the trimming mechanism** (which this doc had failed to describe and is an early-cutoff win), **add `ExecMode`** as the new axis the cone requires; `BuildProfile` was a coarse stand-in and is superseded. §4.1 amended in place.

**4. Snapshot model → state taxonomy — ancestor, evolved in place.** Code: `snapshot_private`/`snapshot_shared` with a coarse `snapshot_key` accelerator and warm reuse implementing `BumpOnContentChange`'s *effect* without its abstraction. Doc: `PhaseSeparated`/`Interleaved` with attestations, epochs, and tier wiring. Ruling: the snapshot model is the **working ancestor** — same intent, missing the trust machinery. Convergence is evolutionary: `snapshot_shared` grows into `PhaseSeparated` (producer-key tracking), `snapshot_private` into `Interleaved` (attestation + epoch + Local cap), the warm-reuse manifest into the per-`StateKey` materialization journal. No rip-and-replace.

*Landed (v3 analysis surface, increment 1):* the typed layer exists in `anneal-rules` (`PersistentStateDecl`, `StateKind`, `Attestation`, sealed `StateHandle` mintable only via `RuleContext::declare_state`, `StateActionExt` grants lowering to the snapshot mechanics), with rule-kind key scoping, epoch-in-key revocation, the §2.5 `Read`-of-interleaved rejection, and cross-target idempotence/mismatch checking via the analyzer's `StateRegistry`. The cargo and pnpm rules are converted (their state keys changed derivation — a one-time warm-tree invalidation). Analysis-time queries landed alongside: `RuleContext::query` runs a `QuerySpec` through the executor when the analyzer is wired with one. Still deferred, recorded in `state.rs`'s module docs: single-producer enforcement for phase-separated state, multi-state actions (the action model carries one snapshot), and the produce-vs-mutate tier distinction (every snapshot owner is conservatively Local today).

**5. Cross-process locking — known gap, part of convergence.** Code: in-process per-key mutexes only (tracked as unsafe under concurrent `anneal` processes). Doc §6.4: per-`StateKey` advisory flocks with PID+boot-id staleness. Ruling: the doc's design stands; the flock upgrade is convergence work, not a design question. Until it lands, §6.4's concurrency story is *design*, and the code comment is the truth.

### Sequencing (decided)

**Spike now, M1 remainder in parallel, then evolve in place.** The §10 spike (both bootstrap-split queries through real `anneal-exec`: stdout capture, constant sandbox roots, journal contact) starts immediately — it is small and de-risks everything downstream. Remaining M1 items (CI cache integration, benchmark gates as release criteria, read-enforcement) proceed alongside; the demo gate is not blocked on convergence. The v3 rule-API migration is **in-place evolution** of the existing crates — current rules and tests stay green at every step — not a parallel build or big-bang rewrite. Convergence order after the spike: trust plumbing on the existing exec layer first (tiers, grades, provenance — they need no rule-API changes), then the v3 analysis surface (sealed handles, state taxonomy per ruling 4, queries), then `ExecMode` + the cone, then the persistence ladder. Engineering choices the doc leaves open (salsa vs. hand-rolled memoization, SQLite-WAL vs. redb) are decided at spike contact, defaulting to the doc's named candidates.

*Landed (ExecMode, increment A):* `ExecMode { Incremental, Hermetic }` is the sixth axis (per ruling 3), default `Incremental`, selected per invocation via `--exec-mode`. **§4.4 is enforced**: action validation hard-rejects a private-snapshot owner (the `mutate_state` lowering) under `Hermetic` — shared snapshots and restores stay legal, since Hermetic forbids mutation, not state. The cargo rule grew its Hermetic arm (same actions, no state grant — `Deterministic`, hence Promotable under `Enforced` per the §2.4 table already landed); pnpm consumes no axes, so its keys are mode-stable and its phase-separated produce/read is legal in both modes. Coloring is **degenerate per-invocation** for now — local defaults Incremental, CI passes `--exec-mode hermetic --require-enforced`. Deferred to increment B: per-node coloring from VCS dirty state, `(label, config)` analyzer keying (its prerequisite), and the §4.3 monotonicity assert (meaningful only once mixed-mode graphs exist).
