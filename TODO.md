# Anneal — open work / TODO

> Running list of what's not yet built, beyond what's already in `build-system-design.md` §21.
> Status as of 2026-06-04: cargo_workspace, nickel_eval, pnpm_workspace M1, Nickel→Rust,
> Nickel→TS, CLI build/test/affected/why, warm reuse, input-digest caching, Linux/macOS
> sealed sandbox hardening, and mandatory Nix toolchain manifests are done. Section refs (§)
> point at `build-system-design.md`.

## Current priority queue

1. **Clean correctness edges:** generated-path collision enforcement, one owning workspace per
   package directory, and CI wiring for correctness-neutral verification.
2. **Next performance lever:** treat `vendor/` / `.cargo/` as one lockfile-keyed unit instead
   of enumerating/stat-ing thousands of vendored files every build.
3. **Store lifecycle:** CAS/action-cache/snapshot GC and eviction, then snapshot-on-evict.
4. **Rule completeness:** cargo metadata/staged graph, cargo test target split/selection, and
   pnpm external-dependency/offline-store work.
5. **CLI/product surface:** `materialize`, `run`/`check`, cache commands, `status`, and richer
   structured test output.

## Correctness & enforcement (do alongside the relevant feature)

- [x] **Sealed sandbox contract + regression tests.** Linux sealed actions use bubblewrap with strict
      filesystem visibility, private network by default, private `/proc`/`/dev` surfaces as documented,
      normalized UID/GID, no inherited stdio/fds, declared roots read-only, and Docker-backed Linux tests.
      macOS sealed actions use `sandbox-exec` with scrubbed env, network denial/allowance by capability,
      undeclared read/write denial, fd hardening, and explicit documentation of non-Linux hermeticity gaps.
- [x] **First-party toolchain resolution is manifest-only.** `nix develop` exports
      `ANNEAL_TOOLCHAIN_MANIFEST`, a Nix-built JSON manifest for `rust`, `posix-runtime`, `node`, and
      `nickel`. `anneal-rules` parses it once per process, validates `/nix/store/...` tool paths and
      closure roots, derives identity from resolved paths + roots, and fails closed if the env var is
      missing. Ambient `PATH` discovery and analysis-time `nix-store -qR` are gone.
- [x] **Generated-path collision enforcement.** Build a `path → producer` map during analysis; error if
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

## Phase 4 — cross-language routing (M1 done; deferred enhancements remain)

- [x] **`pnpm_workspace` M1** — the second native ecosystem and the §14.3 Nickel→TS consumer. Full scope +
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
- [x] **The official §14.3 demo**: Nickel JSON → pnpm workspace, consumed by **relative path** (plain-path,
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
  - [x] **Instrument the analysis front-end.** `anneal-rules` emits analysis subspans used by
        `anneal-bench`, which exposed first-party toolchain resolution as the no-op hot path. The Nix
        manifest path removed that cost: `cargo_workspace.toolchain.rust` / `runtime` are now ~0.1-0.2 ms
        each, and N=16 no-op rebuilds are ~3 ms.
  - [x] **Warm-sandbox reuse for snapshot owners — implemented, now ON BY DEFAULT** (`warm_reuse(false)` opts
        out), per the `docs/sandboxing.md` §5 design. Snapshot owners keep their `target/` working tree in place and sync only
        changed declared inputs (the §5.5 mtime-safe diff: distinct-inode copy + fresh mtime); skips restore +
        teardown; commit-record discipline (manifest doubles as it; clear-on-begin/atomic-write-on-commit); same
        snapshot-key owners serialize on the dir, different keys stay parallel. `warm.rs` (engine, unit-tested) +
        `run_warm` (executor) + `tests/warm_reuse.rs` (revert-hazard correctness + warm≡cold agreement). **Measured
        (bench single-package change vs native):** non-warm +91% (N=16) → +203% (N=48) and *diverging*; **warm
        +36% (N=16) → +58% (N=48) and bounded** — restore+teardown off the critical path. Still not *beating*
        native on trivial crates (native incremental ≈ cargo's fixed startup there); the residual is the
        synchronous full-`target/` save (next).
  - [x] **Private vs shared snapshots — don't save private per build** (`docs/sandboxing.md` §5.8.1). Done:
        `Action.snapshot_shared` + `.snapshot_private`; cargo `target/` is private; `save = SnapshotBased &&
        (snapshot_shared || !warm_eligible)` (the `!warm_eligible` term keeps the non-warm path saving, since
        test-run's byte-identity cache-hit relies on it there). Rule-declared, **not** a per-graph consumer scan
        (the owner is action-cacheable → "no consumer in this graph → skip+cache" would starve a later
        cache-hitting invocation's consumer). Measured (heavy `syn`, warm single-change): **+62% → +27% vs
        native** by dropping the ~19 ms private save.
  - [x] **Default-on prerequisites** (warm reuse is now the default for owners): (a) **warm-path
        correctness-neutrality verification** — `verify_warm_neutral` + `run_warm_uncached` (warm incremental of
        an edited state vs a clean build of it, path-matched at the per-key warm dir → isolates the sync); a real
        cargo two-crate test (`warm_reuse_build_is_correctness_neutral`) is the mtime-hazard backstop. (b) **the
        cross-process workspace lock** + sandbox-name `pid` token — done (`WorkspaceLock`).
    - [ ] **Run the neutrality gate in CI** — `verify_warm_neutral`/`verify_correctness_neutral` exist and have
          representative cargo tests (incl. a content-revert: `warm_reuse_is_neutral_across_a_revert`, the
          mtime-hazard backstop on the real tool); wire them as a routine CI gate over more rules/actions (it's a
          sampling detector, so breadth matters), and add a triage note for tool-nondeterminism false-positives.
    - [ ] **pnpm neutrality (deferred — vacuous in M1 scope).** A pnpm *warm*-neutral test doesn't fit: the only
          warm owner (`install`) declares no file outputs (its product is the node_modules snapshot) and has no
          incremental-reuse shape (its key tracks the lockfile, not source → unchanged = action-cache hit,
          changed = new key = cold); scripts are *consumers* (`SnapshotConsuming`), not warm. The genuine pnpm
          check is **snapshot-*restore* neutrality for the consumer pipeline** — does a script's output match
          whether node_modules was freshly installed vs snapshot-restored. It needs (a) a non-trivial node_modules
          (a local `file:` dep, or — for free — once external-dep vendoring lands) so the result *depends* on
          node_modules, and (b) a new **consumer-pipeline** harness (`install-fresh → script` vs `restore → script`
          in one sandbox; the current single-action cold-vs-warm can't model it, since "cold" = no node_modules =
          the consumer can't run). Build it when external deps make node_modules non-vacuous.
  - [x] **Decision: snapshots are universally non-portable** (`docs/sandboxing.md` §5.9). A snapshot is local
        materialized working state (`target/`, `node_modules`) — never shipped between machines. Cross-machine
        reuse lives in the **content-addressed layer**: action outputs (path-independent) + ecosystem package
        stores (cargo vendor/registry, pnpm `.pnpm-store`). Ship the store, re-materialize the working tree
        locally. Safe by §1.4 (non-portable costs at most re-derivation). *Not* fixing cargo path-sensitivity —
        that fight needs fixed-path canonicalization + cross-machine reproducibility, fragile and against the
        tool's nature.
  - [ ] **Lift `.pnpm-store` out of the pnpm snapshot** — today `snapshot_paths = [node_modules, .pnpm-store]`
        bundles the *portable* store into the *non-portable* snapshot. Separate it into a portable
        content-addressed cache so cross-machine pnpm is "ship store → `pnpm install --offline` re-links a local
        `node_modules`." Symmetric with cargo's vendor/registry. **The principled form of this is the FOD section**
        (§Hermetic dependency acquisition): `.pnpm-store` is populated by per-package hash-pinned fetches from
        `pnpm-lock.yaml` SRI integrity, making it a portable FOD layer rather than a manually-separated cache.
  - [ ] **Cross-machine cargo: rustc-level compilation cache** (sccache-style, path-normalized) — the portable
        layer that gives *fine-grained* cross-machine cargo reuse. Needed because the build action is coarse
        (whole-workspace inputs), so the action cache only exact-hits an unchanged workspace; any change → full
        `cargo build --workspace`. Keeps `target/` local; rustc invocations hit the shared cache. The real v1.x
        remote-cache piece for cargo.
  - [ ] **Snapshot-on-evict** (private snapshots, after GC exists) — when GC removes a warm dir, snapshot it to
        the CAS first (restorable to the same stable path → incremental), so eviction costs a save *once*, not
        per build. Eviction-recovery insurance without the per-build tax.
  - [ ] **Incremental + background save** (for *shared* snapshots, `docs/sandboxing.md` §5.8.2) — a shared
        snapshot must hit the CAS each build (consumers need it), so make that cheap: incremental (re-`put` only
        changed files via the prior manifest's mtime+size) and off the critical path (background; consumers join
        via the existing owner edge). Ties to §1.4 / §8.2 + materialization throughput.
    - [x] **mtime experiment — done, design assumption confirmed + hazard found.** Warm `target/` + changed
          source with a **fresh** mtime → cargo recompiles *only* that crate (the optimization is viable). But
          cargo's freshness is **mtime-based and content-blind** (rust 1.95): the *same* content change behind a
          **stale** mtime is **silently skipped → stale artifact** (a correctness bug, triggered concretely by
          reverting a file to an old CAS blob). **Hard requirement for the sync:** force-touch every
          content-changed file to a fresh mtime; changed files need distinct-inode placement (macOS `clonefile`
          ok; Linux needs a **copy** for changed files, not a hardlink). Captured in `docs/sandboxing.md` §5.5.
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
  - [x] **Realistic workload (heavy real dep: `syn`, vendored)** — `anneal-bench heavy`. The verdict the trivial
        fixture couldn't give. **Cold build +4% vs native** (was +20–45% on trivial crates → that overhead *was*
        a fixed-cost artifact; on real compile it's noise — overhead gate comfortably met). **Deeper finding on
        incremental:** the non-warm path's "incremental" rebuild is a **near-full recompile** (~2 s, `run` = 96%
        of total) — restore-into-fresh-sandbox doesn't preserve the mtime consistency cargo's incremental needs,
        so cargo rebuilds everything → **+3666% vs native**. **Warm reuse preserves the in-place tree (only the
        edit perturbed) → true incremental → +62%** (89 ms vs 55 ms native, a ~23× speedup over non-warm). So warm
        reuse isn't an optimization, it's what makes incremental *work at all* on a real `target/`; its residual
        is the synchronous full-`target/` save (→ §5.8). No-op 4.3× / fresh-checkout 472× faster.
  - [x] **Real-repo gate (serde, vendored)** — `anneal-bench --repo <ws> --edit <rel-src>` (caller does clone +
        `cargo vendor` + `BUILD`; first run pinned serde `5f0f18b`). Cold **+15%**, warm single-change **+125%**
        (vs non-warm **+1110%** — the path-sensitivity full rebuild, reconfirmed on a real repo), no-op **0.7×
        (SLOWER than cargo)**, fresh-checkout **76× faster**. **The finding the synthetic fixtures HID:** on a
        real, file-heavy repo the dominant Anneal overhead is **input handling**, not the snapshot `save` (save is
        only ~2% here). Two costs, both scaling with *input file count* — which vendored deps blow up (thousands
        of files): (1) **cold materialize** = 190 ms (6% of cold, vs ~0 on syn) hardlinking/cloning the whole tree
        incl `vendor/`; (2) **per-invocation `load+analyze` re-hashes every input file every build** (~40 ms for
        serde, vs ~10 ms on syn) — this is what makes the warm single-change +125% (the residual over native's
        225 ms is mostly re-hash, not build) AND what makes the no-op *lose* to cargo (anneal's 43 ms re-hash >
        cargo's 29 ms fingerprint scan) AND what dampens the cache wins (76× not 472×). **Fix → input-digest
        caching:** cache file digests keyed on `(path, mtime, size)` and re-hash only changed files (the §5.8
        incremental-save trick applied to *input* hashing); separately, treat `vendor/` as a single
        lockfile-keyed unit rather than hashing thousands of rarely-changing files per build. This is now the
        real overhead-gate lever at scale (supersedes the trivial-crate "snapshot save" framing for file-heavy
        repos).
  - [x] **Input-digest caching (mtime+size keyed)** — `Cas::ingest_file(path)` (the cached form of
        `put(&fs::read(path))`) keys `(path, mtime, size) → digest` and, on a match whose blob is still present,
        returns the digest after only a `stat` — never reading/hashing the body. Self-heals via `has()` (a GC'd or
        corrupt blob → fall back to read+hash), persisted atomically under `<store>/digest-cache`, flushed on
        drop, thread-safe; `context.rs`'s two input sites now use it; `reads()` exposes the miss count. Content-
        blind (same trade-off as cargo's fingerprint), documented on the method. **Measured (serde): no-op 42.8 ms
        (0.7×, *lost* to cargo) → 15.0 ms (1.7× faster); fresh-checkout 76× → 192×; warm 1-change +125% → +109%.**
        Cold is structurally unchanged (first build caches nothing).
  - [ ] **Treat `vendor/` as one lockfile-keyed input unit** — now the dominant *remaining* per-build input cost.
        With re-hashing gone, the residual that scales with file count is the rest of `analyze`: walking the whole
        tree (`read_dir` recursion), `stat`-ing every file, and assembling + sorting + hashing the action key over
        thousands of `Artifact`s — almost all of them rarely-changing vendored deps. This is most of the warm
        single-change residual (+109%) and what keeps cold's `materialize` at ~185 ms. Fix: represent `vendor/`
        (and `.cargo/`) as a single unit addressed by `Cargo.lock`'s digest, so an unchanged lockfile means we
        neither enumerate nor `stat` the vendored tree — O(workspace sources), not O(vendored files). Symmetric
        with the pnpm `.pnpm-store` / lockfile story.
  - [ ] **pnpm harness**, then competitor baselines (sccache, Turborepo/Nx, Bazel) and real cross-machine CI
        cold-start (needs the remote cache, v1.x).

## CLI

- [x] **`anneal` binary** (§18) — crate `anneal-cli`, thin orchestration over
      `load_package → Analyzer → execute_graph`. **Done so far:** `build` and `test` (single package;
      `test` summarizes via the rule-agnostic `ANNEAL_TEST_EXIT` marker), `--version`, clean exit codes
      (0 ok / 1 failed / 2 usage), and **config-selection flags** (§6.6): `--platform`, `--opt-level`,
      `--lto`, `--debug-info`, `--sanitizer`, `--coverage`.
  - [x] **`affected` / `why`** are wired through `anneal-cli`.
  - [ ] **`run` / `check`**; `query` / `aquery`; `cache` push/info/clean; `status`.
  - [ ] **Structured per-test output** in `test` (libtest/JSON parse via `anneal-test`) — currently
        pass/fail per test action only.
  - [x] **Multi-package targets** — the CLI now loads the target's transitive package closure
        (`load_closure`), so cross-package deps build.
  - [x] **`materialize`** (§14.4) — make a consuming target's tree view match its sandbox: build the
        generated files its actions consume at tree-shaped paths (the rule-declared
        `Analysis::routed_data`, re-homed workspace-relative by the analyzer) and park them in the
        working tree for IDEs and native tooling (`cargo run` over routed `data` inputs). Executes only
        the **producing subgraph**, so it works precisely when the consumer's own build is broken (the
        debug-natively case). Manifest-tracked (`.anneal/materialized`): digest-compare skips identical
        rewrites (no mtime churn), orphans are pruned, edits are never clobbered or deleted without
        `--force`; `--check` / `--list` / `--clean` round it out. Tree copies are excluded from source
        discovery (`RuleContext::with_materialized`), so they can't shadow the producer's declared
        output or perturb cache/snapshot keys (asserted byte-identical in `materialize_exclusion.rs`).
    - [ ] `alias` does not forward routed data (a dest is package-relative to the consuming target;
          forwarding would re-home it) — materialize the actual target. Output-export ("give me the
          built artifact") is deliberately not this verb: it belongs with `run`/`outputs` and a stable
          out-dir. The §14.6 **staged pass** (generated `Cargo.toml`, etc.) remains separate.
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
- [x] **Multi-process safety: coarse `.anneal/` lock + sandbox-name pid.** Done. (a) Sandbox names now include
      the pid (`<keyhex16>-<pid>-<nonce>`), so concurrent `anneal` processes can't collide. (b) `WorkspaceLock`
      (`anneal-cli/src/lock.rs`): a **coarse exclusive `flock`** on `.anneal/lock`, acquired by mutating commands
      (`build`/`test`) in `analyze_and_run` for the run's duration; a second process prints a waiting message
      (with the holder pid) and blocks. `flock` auto-releases on process death → no stale locks. Read-only
      commands (`affected`/`why`) don't acquire it (they read atomic/content-addressed state, safe alongside a
      build). The CAS/cache/snapshot stores were already multi-process-safe (atomic temp→rename); the lock guards
      the mutable working state warm reuse made shared (warm dirs, sandboxes) + future GC.
  - [ ] **Relaxation (when concurrent builds on one store is a real need):** per-`snapshot_key` cross-process
        locks + a *shared* build lock so non-conflicting builds run in parallel; GC stays exclusive (RW model).
        The pid sandbox-name fix is the groundwork that makes this safe. Not needed for M1.
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

- [x] **Nix-provided first-party toolchain manifest.** `flake.nix` builds `.#toolchain-manifest` and the
      dev shell exports `ANNEAL_TOOLCHAIN_MANIFEST`; `anneal-rules` requires it for first-party rules.
      This closes the old ambient PATH/toolchain identity gap for the built-in rules.
- [ ] **WORKSPACE file + `register_toolchain` / `set_default_platform`** (§19.5). Remaining scope is
      user-facing/custom toolchain registration and default platform selection, not first-party Nix
      toolchain discovery.

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
- [ ] **Staged-graph / deferred analysis — the general "tool computes its own structure" mechanism** (foundational
      v1.x; §14.6 Level-3). Many wrapped tools *compute* the build graph's shape rather than declaring it
      statically: `cargo metadata` (Rust members + target kinds), **`go list -json ./...`** (the exact Go analog),
      **CMake configure / `compile_commands.json`** (C/C++ is a meta-build — the graph is generated), **protoc**
      output sets, **Nx/Turbo/Lerna** JS project graphs, and **Nickel generating *targets* (not just data)** —
      config-as-code that defines the graph. The mechanism: a rule emits a **content-addressed, sandboxed prelude
      query action** (e.g. `cargo metadata --no-deps --offline`), it executes, and a **second analysis pass** reads
      its output to emit the rest of the graph. **Non-staleness is free** (the query is keyed on its source inputs
      → re-runs when manifests change, else cache-hits). Industry-validated shape: Bazel **repository rules** (a
      cached external phase that runs tools, e.g. `crate_universe` runs `cargo metadata`) — *not* running tools in
      pure loading. First adopter: **`cargo_workspace`'s `workspace_crates` swaps from hand-parse → cargo metadata**
      (authoritative member/crate-type resolution, no drift) without touching `analyze()` (it consumes the
      structure abstraction). Cost/why-deferred: the graph isn't fully known until prelude actions run, which
      complicates incrementality + the mental model — justified by **ecosystem breadth**, not one rule. (Supersedes
      the old "wrap-don't-decompose sidesteps deferred analysis" note below — wrapping tools that own their
      structure is exactly what *needs* it.)

## Hermetic dependency acquisition — fixed-output fetch (FOD)

The Nix/Bazel model for pulling external deps over the network *without* losing hermeticity or non-staleness.
A foundational, rule-agnostic capability (sibling of staged-graph above); cargo is the first adopter but the
shape is shared across every package ecosystem.

- [x] **Fixed-output fetch primitive (rule-agnostic, §7) — the "safe network" axis.** `CachePolicy::FixedOutput
      { expected: Digest }` + a `network: bool` action capability (default `false`; `.network()` /
      `.allows_network()`; `.fixed_output()` turns it on). A FOD action's output digest is known *before* it runs —
      that is exactly what licenses the network (Nix fixed-output derivation; Bazel `http_archive(sha256)`).
      `executor.rs::execute()` routes `FixedOutput` to `run_fixed_output`: the cache check is `cas.has(expected)` —
      content-addressed by **output**, not inputs → hit = synthesize the result, **no sandbox / no network / no
      fetch** (cross-build/-project dedup); miss = run with network on (no snapshot restore/save), capture the single
      declared output, verify `== expected` (else hard-fail `FixedOutputMismatch`); exactly one output
      (`FixedOutputArity`). Trivially **§1.4 correctness-neutral** (the hash is the sole arbiter — a bad/compromised
      mirror fails *closed*, never corrupts); a FOD action has **no varying file inputs** (determinism is the hash,
      not inputs → cannot smuggle build state into a network call). Composes with the other policies. 5 tests
      (`tests/fixed_output.rs`). Network capability is now enforced by the sandbox backends: sealed actions deny
      network by default, while fixed-output/network-capable actions get the allow-network profile/namespace.
      Deferred optimization: graph-level dedup of identical-`expected` nodes (the `cas.has` short-circuit already
      makes the duplicate a no-op).
  - [x] **Native fetch (no curl, no sandbox, no host trust).** A FOD action may carry a declarative
        `fetch_url` (`ActionBuilder::fetch(url, expected)`: empty command, no inputs/toolchains, one output —
        validated at build time); `run_fixed_output` downloads it **in the executor process** over ureq →
        rustls/ring with Mozilla's roots compiled in (`webpki-roots`). Kills the curl CA failure class
        outright (Nix curl → Nix OpenSSL whose baked OPENSSLDIR ships no CA bundle; the scrubbed sandbox env
        has no `SSL_CERT_FILE` → exit 60 on every fetch) and the per-fetch sandbox spawn; proxies work again
        (the executor keeps the user's env). The pin still carries integrity — embedded roots are
        availability-only. This is the Buck2/Bazel shape: pinned downloads are kernel infrastructure, not an
        inner tool. `cargo_workspace::fetch_action` flipped over; hermetic local-server tests
        (`anneal-exec/tests/native_fetch.rs`: verify, CAS admit, output-keyed hit without network, 5xx retry,
        4xx no-retry, contract validation). Found+fixed en route: `gzip` was missing from the posix-runtime
        toolchain (GNU tar's `z` execs it — vendor assembly died in-sandbox), and
        `fetch_mode_builds_a_registry_dep_offline` is now **gated on `ANNEAL_NETWORK_TESTS=1` instead of
        `#[ignore]`d** so a networked CI lane actually exercises fetch mode (it rotted silently twice behind
        the ignore). The sandboxed-command FOD form remains for user-authored fetch actions.
- [ ] **The per-ecosystem acquisition pattern (one shape everywhere).** Every modern ecosystem converged on the
      same two things — a lockfile with per-artifact hashes + an internal content-addressed store — which *is* the
      FOD shape: `lockfile {(coord, hash)} → one FOD fetch per artifact → assemble blobs into the tool's offline
      layout → build/consume keyed on the **lockfile digest** (not the dep files)`. The bespoke part per ecosystem
      is narrow and irreducible: *parse this lockfile* + *lay blobs out in this tool's offline layout*; a shared
      **"materialize CAS blobs into a directory per a layout fn"** helper carries the rest. Adopters: **cargo**
      (`Cargo.lock` checksums → registry), **pnpm/npm** (`pnpm-lock.yaml` SRI integrity → `.pnpm-store` — *this
      retires the "lift `.pnpm-store` out of the snapshot" item*; the store becomes a portable FOD-populated layer),
      **go** (`go.sum` → `GOMODCACHE`, `GOPROXY=off`), **python** (`uv.lock` / pip `--require-hashes` → wheelhouse,
      `--no-index`), JVM, **OCI** (`FROM img@sha256:…`), generic `http_archive`. Beyond deps — the primitive is
      "acquire immutable, hash-identified content," so it equally covers **pinned toolchains** (download a Go SDK /
      Node / rustc by sha256 → hermetic, pinned toolchain provisioning — see Toolchains), prebuilt native artifacts,
      ML model weights, golden test fixtures.
- [ ] **Unifying insight + the two shared prerequisites.** anneal's CAS becomes the **meta-store**; each per-tool
      store (`vendor/`, `.pnpm-store`, `GOMODCACHE`, a wheelhouse) is a deterministic **materialized view** of it —
      one source of truth, re-projected into whatever layout each native tool reads offline. This is the portable
      half of the cross-machine story (the "ecosystem package store", §cross-machine above): FOD outputs are
      path-independent + hash-keyed → fetched once per machine, shared across all projects, remote-CAS-shareable
      cross-machine (unlike snapshots, which are local). Two cross-cutting prerequisites — each built once, then the
      *next* ecosystem is cheap: **(a)** the blob→layout **assembly helper**; **(b) staged-graph** for the
      *generated-lockfile* case — a library without a committed lockfile (serde; equally a Go/JS lib) hits the
      §14.6 phase wall, because emitting per-artifact FODs needs the lockfile and the lockfile is a *tool output* →
      deferred analysis. So **staged-graph is the shared prerequisite for the no-committed-lock case across all
      ecosystems**; the committed-lockfile slice ships first without it.
- [ ] **Limits (honest).** FOD ⟺ content is **immutable + a-priori hash-identified**. Where it isn't (a moving git
      branch, a lockfile format lacking per-artifact hashes, an ad-hoc URL) → fall back to coarse
      "deterministic-by-trust, keyed on the lockfile" or `NonCacheable`; the boundary is sharp. git/`path` deps are
      harder (a checkout isn't a single-hash tarball — Nix does "fetch + hash the tree"); do registry deps first.
      Trust model = TOFU at lockfile-generation time, verified forever after (the cargo/Nix posture); vendoring
      keeps the actual bytes in-tree (auditable-in-PR provenance) → FOD is a *trade*, not strictly better. Keep both
      acquisition modes available.
- [ ] **cargo: vendoring vs "more cargo-y" — the offline-layout sub-decision.** Vendoring *the **layout*** stays the
      best target **even under FOD**: the `[source]` replacement makes the build **index-free** (no mutable registry
      index, no network, deterministic), and that index-freedom *is* the hermeticity win. "More cargo-y" = the
      **live registry + sparse/git index** is strictly *worse* — it reintroduces the mutable, network-dependent
      index that vendoring/FOD exist to avoid. So FOD does **not** mean "stop vendoring"; it means: (1) swap
      *acquisition* `cargo vendor` (network-at-setup) → per-crate FODs (hash-pinned, verified, portable); (2) make
      the vendor tree a **shared, per-machine, CAS-materialized store** keyed by checksum (a union across projects;
      `[source]` points at it) — materialized once, **not** per build, and **not** in the action key; (3) key the
      build on the **lockfile digest**. Middle option, if cargo-native *resolution* matters while staying hermetic:
      assemble a **frozen local sparse registry** (`sparse+file://`) instead of a vendor dir — cargo runs its normal
      index path, but the index is a static, pinnable local artifact. Default to the **vendor layout** (simplest
      assembly); reach for local-registry only if a vendor-mode limitation bites; **never** the live registry.
      (Reframes "Anneal-managed vendoring" below — the FOD primitive *is* the action-model extension it was waiting
      for.) **DONE for the committed-lockfile case** (vendor layout, in-sandbox assembly, build keyed on the
      `.crate` digests) — see "Anneal-managed acquisition (fetch mode)" under §cargo_workspace completeness.
      **Remaining:** the once-per-machine *shared vendor store* (today assembly re-runs each cold build), and the
      `sparse+file://` local-registry variant only if a vendor-mode limitation bites.

## cargo_workspace completeness

- [x] **External crates via vendoring — works with NO rule changes.** A workspace with `vendor/` +
      `.cargo/config.toml` (from `cargo vendor`) builds hermetically through the existing rule: `walk_tree`
      already materializes `vendor/`/`.cargo/` (not in `IGNORED_DIRS`) and the sealed `cargo build --offline
      --locked` consumes them. Validated end-to-end via `anneal build` on a `syn`-dependent workspace (all 4
      actions ok, offline). So external-dep *enablement* was effectively already present.
  - [x] **Anneal-managed acquisition (fetch mode) — committed-lockfile case done.** No pre-vendoring needed: a
        workspace with a committed `Cargo.lock` (crates.io deps) and **no** `vendor/` now hash-pin-fetches each
        crate via per-crate FOD (static.crates.io `.crate`, pinned to the lockfile checksum), assembles a vendor
        tree in-sandbox (`tar` + a pinned-checksum `.cargo-checksum.json` + a `[source]`-replacement
        `.cargo/config.toml`), and builds/tests `--offline`. `fetch_plan` / `fetch_action` / `assembly_prelude` /
        `with_crates` in `cargo_workspace.rs`; the build is keyed on workspace sources + the `.crate` digests
        (= lockfile checksums), so it's O(workspace sources) not O(vendored files). The `.crate` blobs are
        CAS-deduped (fetched once per machine), but the **vendor *assembly*** still re-runs each cold build (the
        prelude) — making it a once-per-machine materialized store (§FOD point 3) is the remaining perf step.
        Verified end-to-end (`anneal-analysis/tests/cargo_fetch.rs`, `#[ignore]` = network: cfg-if). Required
        wiring the **network capability through the sandbox** (macOS sealed mode denied network unconditionally;
        a `allows_network()` action now gets an allow-network `sandbox-exec` profile — non-FOD sealed actions still
        deny). Pre-vendored + dependency-free paths unchanged. **Still deferred:** the bench still pre-vendors at
        setup (its convenience); generated-lockfile (library w/o committed lock, e.g. serde) needs staged-graph;
        git/`path`/non-crates.io deps error out (vendor those).
- [ ] **Integration-test multi-binary split** (one binary per `tests/*.rs`).
- [ ] **Separately-addressable test targets** (`//ws:crate_a_test_unit`) — falls out of named output groups + demand pruning.
- [ ] **Per-case test durations** (needs libtest JSON, i.e. a nightly `-Z` path or alternative).
- [ ] **Binary targets / bin unit tests** (currently lib unit/doc/integration only).

## Platform

- [x] **Linux sandbox path** — implemented with bubblewrap: mount/user/pid/ipc/uts/net namespaces,
      read-only declared inputs/toolchain roots, private home/tmp/dev/shm/proc surfaces, normalized uid/gid,
      dropped capabilities, fd/stdio hardening, and hermeticity regression tests under Docker.
- [x] **macOS sandbox hardening** — implemented with generated `sandbox-exec` profiles, scrubbed env,
      network deny/allow by capability, fd/stdio hardening, declared toolchain read-only policy, and tests that
      document both enforced guarantees and unavoidable metadata/runtime visibility gaps.
- [ ] **Wire Linux sandbox Docker tests into CI** so the Linux hermeticity suite runs routinely even when
      developers are on macOS.

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
- Deferred/dynamic analysis (DICE-style, *fine-grained* per-action) — only if we move from *wrapping* engines to
  *being* a fine-grained one (C/C++, our own codegen, ThinLTO). NOTE: distinct from the **staged-graph** item under
  *Loader / analysis infrastructure* — that one (a cached prelude query whose output a second pass reads) is
  *needed precisely to wrap* tools that compute their own structure (cargo metadata, go list, cmake), and is no
  longer "sidestepped by wrap-don't-decompose."
