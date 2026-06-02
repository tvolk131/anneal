# Anneal — open work / TODO

> Running list of what's not yet built, beyond what's already in `build-system-design.md` §21.
> Status as of: cargo_workspace, nickel_eval, pnpm_workspace (install + scripts + plain-path data routing) done;
> Nickel→Rust and Nickel→TS (§14.3, plain-path) routing demos proven. Next: the `anneal` CLI, then Phase 5.
> Section refs (§) point at `build-system-design.md`.

## Correctness & enforcement (do alongside the relevant feature)

- [ ] **Generated-path collision enforcement.** Build a `path → producer` map during analysis; error if
      two actions declare the same output path, or if a generated output **shadows** a source file. Promotes
      §14.4's runtime check to an analysis-time one. Cheap (BTreeMap + overlap scan). Needed once multiple
      generators interact / multi-package loading lands.
- [ ] **One owning workspace per package directory.** Reject two workspace targets sweeping the same dir
      (the degenerate exclusive-ownership violation, §1.5).
- [ ] **Explicit `exclude` on sweeping rules** (input-scoping; future). `cargo_workspace`/`pnpm_workspace`
      sweep `source_tree(".")` wholesale; an opt-in `exclude = [...]` would carve out files the rule shouldn't
      claim (e.g. a sibling `.ncl` owned by another rule), removing spurious cross-rule cache deps. This is the
      **local, explicit** alternative to auto-partitioning a residual rule by its siblings' claims — considered
      and **rejected for non-locality** (a sibling rule silently changing the sweep's inputs/cache; against the
      "explicit inputs" grain Bazel chose). Most of the same effect is already available via **package
      boundaries** (put the carved-out file in its own package). Related, semantics-free: an `anneal owners
      //pkg` **diagnostic** that prints the file→rule mapping — the inspectability benefit without the coupling.
- [ ] **Read-tracking to *enforce* declared inputs** (`docs/sandboxing.md`). Fail on undeclared reads —
      defensive, catches under-declaration. Most valuable on macOS (where it's otherwise silent); on Linux
      it's mostly a better diagnostic (isolation already guarantees it). NOT for relaxing invalidation.
- [ ] **Wire the correctness-neutral verification gate into CI per-PR** (§22). Harness exists
      (`verify_correctness_neutral`); it isn't run automatically yet.

## Phase 4 — cross-language routing (in progress)

- [ ] **`pnpm_workspace`** — the second native ecosystem and the §14.3 Nickel→TS consumer. Full scope +
      design in **`docs/pnpm-workspace.md`**. M1 build order: (1) `install` action (resolve+install, **pnpm ≥
      10, no lifecycle scripts**, cached + `node_modules`/store snapshot keyed `(platform, pnpm major, lockfile
      digest)` — Node version dropped); (2) static introspection of `pnpm-workspace.yaml`/`package.json`(s);
      (3) script discovery + declared `scripts = { name: { kind, outputs? } }` with explicit `kind`;
      (4) script actions **non-cacheable + snapshot-consuming**, sealed by default (Node version is the
      *script* toolchain identity); (5) `data` routing as **plain-path** — the generated file is a direct
      relative-path input to the consuming scripts, placed via a per-edge destination (`label_keyed_string_dict`);
      a §14.6 Level-1 clean edge (`docs/pnpm-workspace.md` §4); (6) axis map (§7 of the doc). Deferred within the
      rule: **name-resolution routing** (`@gen/config` by name; `file:` dep + synthesized wrapper +
      `ctx.generated_file`) — gated on a deps-carrying generated package (the *pass-through* flavor) or the §14.1
      differentiator needing to be visible; sealed+reproducibility-gated cache opt-in for scripts; external
      vendoring; structured JS test-result parsing (exit-based first); explicit native-build actions
      (`node-gyp`-at-install unsupported).
- [ ] **The official §14.3 demo**: Nickel JSON → pnpm workspace, consumed by **relative path** (plain-path,
      M1); composing caches (edit .ncl → consumer rebuilds; edit consumer → generator cached). (Nickel→Rust is
      proven; TS is the named demo.) Name-by-import (`@gen/config`) is the deferred name-resolution enhancement.
- [ ] **Nickel `import`s** (multi-file Nickel) — declare imported files as inputs (currently single self-contained `src`).

## Phase 5 — queries, CI cache, transitions

- [x] **`affected --since=<commit>`** (§11.3) — the primary CI primitive. `owner(path)` + whole-workspace
      load + reverse-dependency closure (`anneal-query`); `anneal affected --since <ref>`. Package granularity;
      unowned change → conservative workspace-wide.
- [x] **`why <from> <to>` + `why <target> --since`** (§11.3) — shortest dependency path (2a):
      `anneal-query::shortest_path`/`why` + `anneal why`. Deterministic via **sorted-label BFS** (stable under
      cosmetic dep reordering; no HashMap-order dependence), tested. Uses `from`'s forward closure, not a
      whole-workspace load.
- [ ] **`why --all` — connecting subgraph** (2c, deferred): the union of *all* paths from A to B as a DAG
      (Bazel's `allpaths`) — complete and graph-size-bounded (vs. the combinatorial explosion of enumerating
      every path), for dependency-entanglement analysis. Needs graph rendering (tree-indent or dot). Worth doing,
      not now.
- [ ] **`affected --explain`** (deferred): annotate each affected target with its reason-path, so the selection
      is self-explaining in CI — removes the manual `affected` → `why` orchestration. Small follow-on once
      `why`'s pathfinding exists (it's "run `why` per affected target").
- [ ] **Untracked-file gap in `affected`** — `git diff --name-only` omits untracked-but-unadded files, so a
      brand-new source file isn't flagged until staged/committed. Consider `--others --exclude-standard` (with
      care to not sweep build junk).
- [ ] **GitHub Actions cache integration** (§8.5) — the adoption wedge (dumb fetch/build/push Action).
- [ ] **Transitions** (§6.4: host_to_exec, target_to_exec, explicit platform) + the **direct platform-transition
      test** (build a bare crate for two target platforms; assert distinct correctly-cached configured targets).
      Currently configuration is a single constant per analysis.

## Phase 6 — validation

- [ ] **Benchmark gates** (§20) — the actual "thesis validated" bar. Incremental must *beat*; cold/workspace-wide
      must *match*; CI cold-start must *beat*.
  - [x] **First cargo harness** — `crates/anneal-bench` drives the library pipeline over N dependency-free crates
        vs. the exact native cargo invocation the rule wraps. Three gates: cold (overhead), no-op, and
        fresh-checkout-with-warm-cache (the locally-measurable CI cold-start beat — populated `.anneal/` restores
        from CAS while cargo with no `target/` rebuilds). Run: `cargo run -p anneal-bench --release [-- N]`.
        **Cache wins confirmed**: no-op ~10–55× faster, fresh-checkout ~90–300× faster (cache hit skips cargo
        entirely). **Finding to chase**: cold overhead is **not fixed — it grows with crate count** (+20% at N=2 →
        +45% at N=32 on trivial crates), because Anneal = `cargo` + materialize-sources + **save `target/`
        snapshot**, and the snapshot save scales with `target/` size while trivial crates give it nothing to
        amortize against. Trivial crates are the *pessimal* case for the overhead gate; realistic compile times
        would shrink the %, but the absolute snapshot cost on a large `target/` is real (ties to §8.2 / deferred
        deep snapshot pruning).
  - [x] **Instrument the phase breakdown** — `LocalExecutor::record_timings()` → `PhaseTimings` per executed
        action (materialize / restore / run / capture / save / teardown), surfaced by the bench. **Confirmed
        (with nuance):** the in-execution wrapping overhead is dominated (~75%) by **two `target/`-size-scaling
        phases — snapshot `save` (~1 ms/crate) and sandbox `teardown` (`rm -rf` of a target/-laden sandbox,
        ~0.55 ms/crate)**; both grow ~linearly with output size. `run` (cargo itself) is 73–92% of total;
        materialize+capture are ~2% each. Separately, load+analyze adds a roughly-fixed ~10 ms to a cold
        `anneal build`. Trivial crates *exaggerate* this (no compile time to amortize), but `save` scales with
        `target/` **byte** size, and real `target/` dirs are large — so this is the overhead-gate lever at scale.
  - [ ] **Attack the O(`target/`) incremental overhead** (the lever for the "must beat" gate; prioritize once a
        realistic workload confirms the absolute cost matters). **Design captured in `docs/sandboxing.md` §5**
        (reusability-iff conditions, three-tier fallback, dirty-state clean-commit marker, source-sync diff,
        at-rest structure, mtime edge). Biggest: **warm-sandbox reuse for snapshot *owners*** — keep the build
        action's `target/` working dir in place across incremental rebuilds (skip restore + teardown, ~25 ms
        @ N=16) instead of the fresh-sandbox + snapshot round-trip every run; owners reuse (one per key) while
        consumers stay unique+parallel. Plus **incremental + background snapshot save** (re-`put` only changed
        files, attacks the ~7 ms save). **Validate first:** the mtime experiment (§5.5) — restore a warm
        `target/`, touch one source, confirm cargo does *minimal* incremental, not full or under-rebuild. Ties to
        §1.4 / §8.2 + the materialization-throughput item.
  - [x] **Single-package-change scenario** — the canonical incremental case, added to the harness (edit one
        crate, rebuild). **This is the §20.3 "incremental must *beat*" gate, and on this fixture Anneal *loses*,
        worse as the workspace grows: +50% (N=4) → +160% (N=32) → +265% (N=64) vs native cargo.** Structural
        reason (confirmed by the incremental phase breakdown): native cargo does **O(change)** work — recompile
        one crate, keep `target/` in place — while Anneal does **O(full `target/`)** work every build: restore the
        whole snapshot (~16 ms @ N=16) + recompile + save the whole snapshot (~7 ms) + teardown the sandbox
        (~9 ms). So the overhead is independent of how small the change is, and the ratio diverges with `target/`
        size. **Caveat:** trivial crates make native incremental ~instant (≈cargo's fixed startup), so there's no
        real compile to amortize the O(`target/`) overhead against; on real crates (seconds/recompile) the
        absolute overhead may be acceptable — *or not*, if `target/` is GB-scale. **Only a realistic-`target/`
        workload resolves this** (→ real-repo benchmark).
  - [ ] **Realistic workloads** — crates with real compile time (and eventually external deps) so the overhead
        gate is assessed where compile dominates, not snapshot bookkeeping.
  - [ ] **pnpm harness**, then competitor baselines (sccache, Turborepo/Nx, Bazel) and real cross-machine CI
        cold-start (needs the remote cache, v1.x).

## CLI

- [x] **`anneal` binary** (§18) — crate `anneal-cli`, thin orchestration over
      `load_package → Analyzer → execute_graph`. **Done so far:** `build` and `test` (single package;
      `test` summarizes via the rule-agnostic `ANNEAL_TEST_EXIT` marker), `--version`, clean exit codes
      (0 ok / 1 failed / 2 usage), and **config-selection flags** (§6.6): `--platform`, `--opt-level`,
      `--lto`, `--debug-info`, `--sanitizer`, `--coverage`.
  - [ ] **`run` / `check`**; `query` / `aquery` / **`affected` / `why`** (Phase 5); `cache` push/info/clean;
        `status`. (`affected`/`why` need multi-package loading.)
  - [ ] **Structured per-test output** in `test` (libtest/JSON parse via `anneal-test`) — currently
        pass/fail per test action only.
  - [x] **Multi-package targets** — the CLI now loads the target's transitive package closure
        (`load_closure`), so cross-package deps build.
  - [ ] **`materialize`** (§14.4) — write generated native packages/files to stable on-disk paths for IDEs and
        native tooling. Also the mechanism for the §14.6 **staged pass** (generated `Cargo.toml`, etc.).
  - [ ] **`exec`** escape hatch (§7.6) — run an arbitrary command in a sandbox (permissive by default;
        `--hermetic`/`--no-network` opt-in).
  - [ ] **`init` / `init --detect`** (§15.2) — interactive setup / scaffold config without touching native files.

## Performance & scale

- [x] **Parallel action execution.** `execute_graph` now schedules the action DAG **concurrently** (bounded
      `std::thread::scope` worker pool, `--jobs` / `available_parallelism`). DAG derived in `build_edges` from
      `InputSource::Output` data edges **+ snapshot-owner edges** (each `SnapshotConsuming` action depends on the
      `SnapshotBased` owner of its `snapshot_key`); input slice need not be topological; **unique sandbox per
      action** (dropped the shared stable `snap-K` path — the snapshot is an immutable shared read from the store
      into each private sandbox, so consumers parallelize freely, ordered only by the owner edge); results aligned
      with the input by index; a dependency cycle → `ExecError::DependencyCycle`; an execution *error* aborts new
      dispatch and drains in-flight (a non-zero *exit* stays a normal result). Tests in `parallel_scheduler.rs`
      (out-of-order diamond, rendezvous proving real overlap, error/cycle paths). Deferred: lifting the
      snapshot-owner edge into the action model (see triggers below); see also the multi-process hardening item.
- [ ] **Multi-process safety: coarse `.anneal/` lock + sandbox-name collision** (hardening; not needed for the
      single-process parallel scheduler). (a) Normal sandbox names are `<keyhex16>-<nonce>` where `nonce` is a
      per-**process** in-memory `AtomicU64` from 0 → **two `anneal` processes collide** on the same path and
      `rm -rf` each other (latent today, snapshot-independent). Fold a `pid`/random token into the name. (b) Add a
      workspace-level **RW advisory lock** (flock on a lockfile): builds take **shared**, GC takes **exclusive**
      (Cargo/Bazel pattern). Subsumes every cross-process concern: the CAS is already corruption-safe
      (content-addressed + atomic temp→rename), so only deletion (GC) needs exclusivity, not all CAS access.
- [ ] **CAS / action-cache / snapshot eviction & GC** (§8.2). All three stores currently grow **unbounded**.
      The system owns eviction policy (LRU, size/age caps); rules declare only what to prune. Design: blobs are
      **shared** across action-cache entries and snapshots, so deletion is **not** the inverse of write — removing
      a pointer (snapshot index entry / action-cache entry) is an atomic unlink, but removing a **blob** is
      **reachability-gated GC** (mark-and-sweep over all live pointers → referenced digests; a blob is deletable
      only when unreachable from every root). Run GC **stop-the-world under the exclusive workspace lock** so it
      can't race a concurrent build committing a new referrer to a blob it's about to sweep (the git/Nix/Bazel
      GC-vs-write race). Builds hold the shared lock; GC holds exclusive.
- [ ] **Materialization throughput** on a real `.anneal` volume (Spike B carried-forward: ~4,600 clones/sec in
      the harness). Benchmark and, if material, batch/parallelize.

## Toolchains & configuration

- [ ] **WORKSPACE file + `register_toolchain` / `set_default_platform`** (§19.5). Toolchains are currently
      discovered ad-hoc by scanning `PATH` (in `cargo_workspace`/`nickel_eval`); replace with explicit,
      content-addressed registration so the toolchain is a declared input, not ambient.

## Provider / variant model (designed §5.5–5.6, build when needed)

- [ ] **Named output groups + dependent variant selection** — Milestone 1 ships only the default group.
- [ ] **Demand-driven output pruning** — build only the provider outputs a build consumes; the enabler for
      non-wasteful multi-variant menus. Also subsumes separately-addressable test targets.
- [ ] **Tree / directory artifacts** — outputs whose member set is known only at execution (e.g. generated
      package dirs). Dir-walk machinery already exists in the snapshot protocol.
- [ ] **Typed metadata providers** (`RustLibraryInfo`, `ToolchainInfo`, …) — concept only so far.
- [ ] **`data` placement: default + per-edge override** (DECIDED). The consuming rule owns the placement
      *policy*; `data` accepts an optional per-edge destination — `data = [("//x:cfg", "gen/config.json")]` —
      defaulting to the artifact's own path when omitted. A useful *default* earns its keep only where the
      consumer can reference the result **symbolically**: pnpm by package **name**, genrule by **`$(location)`**,
      cargo `build.rs` via **`OUT_DIR`/`CARGO_MANIFEST_DIR`**. Bare `include_str!` (a literal path, no
      indirection) → **explicit per-edge placement**, not a default-to-conform-to. Do **not** use `env!()` in
      source (cache-key churn, no gain).
  - [ ] Add the optional per-edge destination to `cargo_workspace`'s `data` (default = artifact path).
  - [ ] Update the `nickel_to_cargo` demo/test to use explicit per-edge placement — it currently relies on the
        producer's `out` path, which couples the consumer's `include_str!` to the producer.
  - [ ] `genrule` `$(location //x)` make-var (symbolic reference for command-driven consumers).

## Loader / analysis infrastructure

- [x] **Multi-package workspace loading** — `load_closure(root, target, registry)` walks the transitive
      package closure from a target (loads only reachable packages, merges into one TargetGraph); the analyzer
      is unchanged (single-graph consumer). The CLI uses it, so cross-package targets build. Remaining: the
      ownership `owner(path)` walk + `affected` build on this (Phase 5); whole-workspace enumeration (for
      `query //...`) is separate from this on-demand closure.
- [ ] **`load()` of `*.bzl` libraries** (§4.4) — shared Starlark, reserved but not implemented.
- [ ] **Restricted user-facing subset linter** (§4.2).
- [ ] **starlark-rust monorepo-scale perf** validation (Spike A deferred check, §22).

## cargo_workspace completeness

- [ ] **Dependency vendoring** — workspaces with external crates (currently `--offline --locked`, no-dep/path-dep only).
- [ ] **Integration-test multi-binary split** (one binary per `tests/*.rs`).
- [ ] **Separately-addressable test targets** (`//ws:crate_a_test_unit`) — falls out of named output groups + demand pruning.
- [ ] **Per-case test durations** (needs libtest JSON, i.e. a nightly `-Z` path or alternative).
- [ ] **Binary targets / bin unit tests** (currently lib unit/doc/integration only).

## Platform

- [ ] **Linux sandbox path** — mount namespaces + read-only bind mounts. Currently a `cfg` stub; only macOS
      (clonefile CoW + sandbox-exec) is exercised. Linux gives strict, kernel-enforced hermeticity.

## Diagnostics

- [ ] **Diagnostics channel** (§17.2/§19.3) — schema defined in the doc; no crate, rules don't emit `Diagnostic`s.
      (Structured *errors* for load/exec exist; the diagnostics *channel* is separate.)
- [ ] **Stable error codes + `anneal explain MB0023`** (§17.1) — we produce structured, located errors, but
      there's no stable code registry or doc-linked long-form `explain`.

## Deferred by design (v1.x+ / out of Milestone 1)

- Remote cache backend, then remote execution (§9).
- Persistent TypeScript worker (§10).
- `nextjs_app`; **Rust → WASM → TS** typed bindings (§13.5) — the typed cross-language demo.
- `uv_workspace` (Python), `go_module` (Go).
- Secrets / private registries (§7.5) — deliberate non-goal for M1.
- Deferred/dynamic analysis (DICE-style) — only if we move from *wrapping* engines to *being* a fine-grained one (C/C++, our own codegen, ThinLTO). See chat: wrap-don't-decompose largely sidesteps the need.
