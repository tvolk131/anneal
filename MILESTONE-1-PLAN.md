# Anneal — Milestone 1 Implementation Plan

> Companion to `build-system-design.md`. That document defines *what* the Thesis
> MVP is and *why*; this one defines the *order* we build it, the *exit criteria*
> for each phase, the *crate decomposition*, and the *verification harness* that
> guards the central correctness invariant.
>
> Status: pre-implementation. Section references (§) point at `build-system-design.md`.

---

## 0. Guiding principle

**Front-load the claims that can kill the thesis, not the work that is merely large.**

§22 names the assassins — starlark-rust integration, snapshot correctness-neutrality,
and the macOS materializer. §20 declares the thesis *unvalidated* until incremental
builds beat native tooling. So the sequence chases those signals early via two
throwaway **spikes** (Phase 0) and two **vertical slices** (Phases 3–4) rather than
building the whole stack bottom-up and discovering a fatal assumption at the end.

Two structural commitments shape everything below:

1. **Define seams as traits before filling them.** `Executor`, `Rule`, the CAS, and
   the diagnostics sink are defined as narrow interfaces in Phase 1–2 even though
   only one implementation exists in Milestone 1. This is the deep-module discipline
   (see §6 of this doc) and it is what lets remote execution / workers slot in during
   v1.x without a rewrite.
2. **Build the correctness-neutral verification harness *with* the first snapshot,
   not after it.** §22 prescribes "cold build vs. snapshot-restored build, diff
   outputs … treat any divergence as a release blocker." That harness is a Phase 3
   deliverable, not a Phase 6 afterthought.

---

## 1. Toolchain & environment

Provided by `flake.nix` (committed). Enter with `nix develop` or run one-off commands
with `nix develop --command <cmd>`. Pinned via `flake.lock`.

| Tool | Version (current lock) | Used by |
|------|------------------------|---------|
| rustc / cargo | 1.95 | build system itself + `cargo_workspace` |
| nickel | 1.16 | `nickel_eval`, the Nickel → TS routing demo |
| node | 22 | `pnpm_workspace` consumer side |
| pnpm | 11 | `pnpm_workspace` |

The build system is itself a Cargo workspace (§16.1), so `cargo build` is always the
fallback and there is no bootstrap problem.

---

## 2. Phase plan

Each phase lists its **goal**, **work items**, and a hard **exit criterion**. A phase
is not "done" until its exit criterion is demonstrable.

### Phase 0 — De-risking spikes (throwaway)

**Goal:** convert §22's two highest-risk assumptions into facts before committing
architecture to them. Code here is disposable; it informs the real interfaces.

- **Spike A — starlark-rust.** Load a trivial `BUILD` file; register a rule primitive
  as a Starlark global; trigger an evaluation error. Confirm we can (a) map a
  diagnostic back to the user's source location (file/line/col) and (b) inject a
  custom error message. This underpins the entire loading layer (§4).
- **Spike B — CAS + hardlink materializer on macOS/APFS.** Hardlink N files from a
  CAS-shaped directory into a sandbox root. Probe: per-inode hardlink limits, locking,
  same-filesystem requirement, cross-filesystem copy fallback (§3.4, §22).

**Exit criterion:** a written findings note for each spike — does the assumption hold,
and what constraints does it impose on the real interface? (e.g., "starlark-rust
exposes `FrozenModule` + `CallStack` with spans; source locations survive freezing"
or "APFS hardlink limit is N; CAS and sandbox must share a volume").

### Phase 1 — Execution kernel (bottom-up foundation)

**Goal:** run *one* action hermetically and cache it. No graph, no Starlark yet.

- `anneal-core`: `Digest`, `Label` (§19.4 grammar), `Platform`, `Configuration` + the
  five axes (§6.2). Newtypes with private fields + validating constructors.
- `anneal-cas`: content store — `put / get / has`.
- Action model (§19.1) + action cache (§8.1), including **axis trimming** in the
  digest (only consumed axes enter the key).
- Materializer + sandbox (§3.4, §7.3). macOS `sandbox-exec` path done honestly; Linux
  mount-namespace path behind the same `cfg` seam.
- `Executor` trait + `LocalExecutor` producing `ActionResult` (§7.1). Defined as a
  trait now even though only local exists in M1.

**Exit criterion:** hand the kernel a hand-written action spec → get a sandboxed,
content-addressed, cached result; re-running the identical action is a cache hit with
no re-execution.

### Phase 2 — Graph layers + trivial rules

**Goal:** user-written `BUILD` files become an executable action graph.

- `Rule` trait (§5.2): `attrs × config × deps → actions + providers`. The system/rule
  interface (§5.3) — exists before any rule.
- `anneal-loader`: Starlark → target graph; schema validation at the rule boundary
  (§4.3); structured errors with source locations (§17.1) threaded from the start.
- `anneal-analysis`: configured target → action graph; provider propagation; config
  resolution + the three transitions (§6.4).
- `filegroup` / `alias` / `genrule`. `genrule` is the first end-to-end "real action
  through the executor" test — cheap, high signal.

**Exit criterion:** a `genrule` written in a real `BUILD` file loads, analyzes, and
executes through the Phase 1 kernel, producing a cached output.

### Phase 3 — Vertical slice #1: `cargo_workspace` (riskiest thesis claim)

**Goal:** validate the **correctness-neutral snapshot invariant** (§1.4, §8.2) and
produce the **first benchmark-gate number** (§20.3). Drive one native rule fully
through the stack before broadening.

- `cargo_workspace`: coarse build/check actions (§3.2); per-`(crate, test_type)` test
  targets with the compile/run split (§12.2–12.3); conservative whole-`target/`
  snapshot (§8.3).
- **The correctness-neutral verification harness** (see §4 of this doc) — built here.

**Exit criterion:** (1) a multi-crate Cargo workspace builds and tests through Anneal;
(2) the verification harness shows a snapshot-restored build produces byte-identical
outputs to a cold build (modulo declared non-determinism); (3) a first incremental
benchmark number exists against raw `cargo` + GitHub Actions cache.

### Phase 4 — Vertical slice #2: cross-boundary routing

**Goal:** the second headline claim — generated-native-package routing across an
ecosystem boundary (§14.1–14.3), with composing caches.

- `nickel_eval` first (pure, consumes no axes — simplest generator, §13.6).
- `pnpm_workspace` (node_modules materialization; pnpm-store snapshot).
- Routing: Nickel JSON shaped as a generated native package (with `package.json`) →
  consumed by pnpm as an ordinary workspace dependency; content-addressed under
  `.anneal/gen/` (§14.2).

**Exit criterion:** edit Nickel source → consumer regenerates + rebuilds; edit only
the consumer → generator stays cached. Demonstrated as a test.

### Phase 5 — Queries, CI, transition test

**Goal:** the CI primitives and the one piece of config machinery no M1 rule exercises.

- `affected --since=<commit>` + `why <target>` (§11.3). `owner(path)` is a filesystem
  lookup thanks to package-level ownership.
- GitHub Actions cache integration (§8.5) — the adoption wedge (§15.1). Dumb Action,
  intelligence in the build system.
- Direct platform-transition test (§6.4): a bare Rust crate built for two target
  platforms → assert distinct, correctly-cached configured targets and correct outputs.

**Exit criterion:** `affected` selects the correct target set across a diff; the
GitHub Action restores/saves precisely; the transition test passes.

### Phase 6 — Benchmark gates (thesis validation)

**Goal:** the actual definition of "done" (§20).

- Run the full scenario × baseline matrix (§20.1–20.2) on representative repos.
- Apply the pass criteria (§20.3): incremental must *beat*; cold-cache /
  workspace-wide must *match* within margin; CI cold-start must *beat*.

**Exit criterion:** the §20.3 criteria are met, or a documented decision to revisit the
design (§20 closing paragraph).

---

## 3. Crate decomposition (deep modules)

Ousterhout's metric: module value = functionality hidden ÷ interface surface. Crates
are Rust's strongest module boundary. The carving names deep modules the design
already implies (the §7.1 unified executor, §3.2 Cargo-as-opaque-engine, the §8.2
snapshot protocol).

```
anneal-core         vocabulary: Digest, Label, Platform, Configuration, axes
   |  (everyone depends on this — keep it THIN: newtypes, private fields)
anneal-cas          content store              put / get / has
anneal-exec         Executor + materializer + sandbox + action cache
anneal-snapshot     save / restore / prune + correctness-neutral verification
   |
anneal-loader       Starlark -> target graph   (hides starlark-rust ENTIRELY)
anneal-analysis     configured target -> action graph; providers; transitions
anneal-rules        Rule trait + all first-party rules (each rule a deep submodule)
anneal-diagnostics  structured errors + Diagnostic + rust-style rendering
anneal-query        query / affected / why
   |
anneal-cli (anneal)  thin orchestration — allowed to be a coordinator
```

| Crate | Narrow interface | Hidden complexity |
|-------|------------------|-------------------|
| `anneal-cas` | `put(bytes)→Digest`, `get`, `has` | on-disk layout, sharding, GC, locking, hardlink mechanics |
| `anneal-exec` | `Executor::execute(Action)→ActionResult` | sandbox setup, materialization, OS isolation behind `cfg`, env scrubbing, cache-key computation, output capture |
| `anneal-snapshot` | `save / restore / prune(key)` | content-addressing mutable trees, eviction, neutrality verification |
| `anneal-loader` | `load(workspace)→TargetGraph` | all of starlark-rust, the restricted subset, schema validation, source-location mapping |
| `anneal-analysis` | `analyze(ConfiguredTarget)→ActionGraph` | config resolution, the three transitions, provider propagation, memoization |
| `anneal-rules` | the `Rule` trait | each rule hides a tool's *entire* invocation (Cargo.toml introspection, JSON parsing, test-type detection, snapshot wiring) |
| `anneal-diagnostics` | structured `Error` / `Diagnostic` + one render entry point | aggregation, dedup, causal-chain collapsing, compiler-style formatting |

**Design rules for the decomposition:**

- The **§5.3 system/rule boundary is the most important deep-module interface.** "System
  provides policy; rules provide mechanism" *is* the depth principle. Maintain it by
  discipline now, while rules are first-party.
- **`anneal-core` is the one shallow-module risk** — a "vocabulary" crate degenerates
  into a junk-drawer of public-field structs. Discipline: newtypes with private fields
  and validating constructors; keep it small; behavior belongs in the deeper module
  that owns the concept.
- **Do not over-split.** Keep materializer + sandbox + action-cache *inside*
  `anneal-exec` as private modules until a second consumer appears. Splitting adds
  interface surface without hiding new complexity — that manufactures shallow modules.
  Smell test: if a crate's public functions mostly forward to another crate's functions
  of the same shape, the boundary is shallow — collapse it.
- `anneal-snapshot` is kept separate from `anneal-cas` because the *protocol*
  (save/restore/neutrality) is a genuinely different abstraction from blob storage.

---

## 4. The correctness-neutral verification harness (§1.4, §8.2, §22)

The central invariant: **a snapshot may make a build faster but must never change its
semantic output.** This harness is the release-blocker gate that proves it, built in
Phase 3 alongside the first snapshot.

**Mechanism:**

1. Run a target **cold** (fresh workdir, empty native cache) → capture all declared
   outputs, hash each into the CAS.
2. Run the same target **warm** (snapshot restored into a fresh workdir) → capture and
   hash the same outputs.
3. **Diff the output digests.** Any divergence is a release blocker, not a warning.

**Coverage to build in:** byte-identical comparison of build artifacts and test
binaries; an allowlist for *declared* non-determinism (e.g., embedded version strings,
timestamps) so the diff is meaningful rather than perpetually red; runs in CI on every
PR (ties into the §16.1 self-hosting / double-bootstrap discipline).

**Why Phase 3, not later:** a restored `target/` that subtly changes output is "a
severe, hard-to-detect bug" (§22). Detecting it requires the harness to exist the
moment the first snapshot does.

---

## 5. Risk register (tracking §22)

| Risk | Retired by | Status |
|------|-----------|--------|
| starlark-rust integration | Spike A | **retired** — see `spikes/FINDINGS.md` |
| macOS materializer at scale | Spike B | **retired** (hardlink throughput carried to Phase 3 benchmarks) |
| macOS read-only / store-corrupting inputs | APFS `clonefile` materialization | **addressed** — inputs are copy-on-write clones (distinct inode) marked `0444`; a write to an input COWs and cannot corrupt the store (proven by test). Full *read*-isolation of undeclared files remains the Linux-VM path. |
| snapshot correctness-neutrality | Phase 3 verification harness | **harness built & passing** — `anneal-snapshot` + `verify_correctness_neutral`; a warm (incremental) build that reuses a snapshotted crate produces byte-identical outputs to a cold build. Wired into CI per PR is the remaining step. |
| per-`(crate, test_type)` cold-cache overhead | Phase 6 benchmarks (batch-invocation fallback ready) | pending |
| macOS hermeticity ceiling | documented; Linux-VM mode for strict needs | accepted/documented |
| system/rule boundary under future extensibility | discipline now, while first-party | ongoing |
