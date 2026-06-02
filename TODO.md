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

- [ ] **`affected --since=<commit>`** and **`why <target>`** (§11.3) — the primary CI primitive. Depends on
      multi-package loading + the exclusive-ownership `owner(path)` lookup.
- [ ] **GitHub Actions cache integration** (§8.5) — the adoption wedge (dumb fetch/build/push Action).
- [ ] **Transitions** (§6.4: host_to_exec, target_to_exec, explicit platform) + the **direct platform-transition
      test** (build a bare crate for two target platforms; assert distinct correctly-cached configured targets).
      Currently configuration is a single constant per analysis.

## Phase 6 — validation

- [ ] **Benchmark gates** (§20) — the actual "thesis validated" bar. Untouched. Incremental must *beat*;
      cold/workspace-wide must *match*; CI cold-start must *beat*.

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

- [ ] **Parallel action execution.** `execute_graph` runs actions **sequentially**; independent actions should
      run concurrently (with snapshot-key serialization for same-key snapshot actions). Significant for the §20
      gates.
- [ ] **CAS / action-cache / snapshot eviction & GC** (§8.2). All three stores currently grow **unbounded**.
      The system owns eviction policy (LRU, size/age caps); rules declare only what to prune.
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
