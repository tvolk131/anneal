# Anneal — open work / TODO

> Running list of what's not yet built, beyond what's already in `build-system-design.md` §21.
> Status as of: cargo_workspace + nickel_eval done; Nickel→Rust routing proven; pnpm_workspace not started.
> Section refs (§) point at `build-system-design.md`.

## Correctness & enforcement (do alongside the relevant feature)

- [ ] **Generated-path collision enforcement.** Build a `path → producer` map during analysis; error if
      two actions declare the same output path, or if a generated output **shadows** a source file. Promotes
      §14.4's runtime check to an analysis-time one. Cheap (BTreeMap + overlap scan). Needed once multiple
      generators interact / multi-package loading lands.
- [ ] **One owning workspace per package directory.** Reject two workspace targets sweeping the same dir
      (the degenerate exclusive-ownership violation, §1.5).
- [ ] **Read-tracking to *enforce* declared inputs** (`docs/sandboxing.md`). Fail on undeclared reads —
      defensive, catches under-declaration. Most valuable on macOS (where it's otherwise silent); on Linux
      it's mostly a better diagnostic (isolation already guarantees it). NOT for relaxing invalidation.
- [ ] **Wire the correctness-neutral verification gate into CI per-PR** (§22). Harness exists
      (`verify_correctness_neutral`); it isn't run automatically yet.

## Phase 4 — cross-language routing (in progress)

- [ ] **`pnpm_workspace`** — the second native ecosystem and the §14.3 Nickel→TS consumer. node_modules
      materialization, pnpm offline store snapshot, "generated native package" shaping (package.json).
- [ ] **The official §14.3 demo**: Nickel JSON → pnpm workspace imported as a module; composing caches
      (edit .ncl → consumer rebuilds; edit consumer → generator cached). (Nickel→Rust is proven; TS is the named demo.)
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

- [ ] **`mybuild` binary** (§18) — everything is currently driven via the Rust library/tests. Needs
      build/run/test/check, query/affected/why, materialize/exec, cache, init.

## Provider / variant model (designed §5.5–5.6, build when needed)

- [ ] **Named output groups + dependent variant selection** — Milestone 1 ships only the default group.
- [ ] **Demand-driven output pruning** — build only the provider outputs a build consumes; the enabler for
      non-wasteful multi-variant menus. Also subsumes separately-addressable test targets.
- [ ] **Tree / directory artifacts** — outputs whose member set is known only at execution (e.g. generated
      package dirs). Dir-walk machinery already exists in the snapshot protocol.
- [ ] **Typed metadata providers** (`RustLibraryInfo`, `ToolchainInfo`, …) — concept only so far.
- [ ] **`data` placement policy** — consuming rule owns placement; decide default-vs-explicit + symbolic
      reference (`$(location)` / package name / env var). Open design question (see chat).

## Loader / analysis infrastructure

- [ ] **Multi-package workspace loading** — load several BUILD files into one TargetGraph (currently single
      package). Prereq for cross-package deps, `affected`, and the ownership `owner(path)` walk.
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

## Deferred by design (v1.x+ / out of Milestone 1)

- Remote cache backend, then remote execution (§9).
- Persistent TypeScript worker (§10).
- `nextjs_app`; **Rust → WASM → TS** typed bindings (§13.5) — the typed cross-language demo.
- `uv_workspace` (Python), `go_module` (Go).
- Secrets / private registries (§7.5) — deliberate non-goal for M1.
- Deferred/dynamic analysis (DICE-style) — only if we move from *wrapping* engines to *being* a fine-grained one (C/C++, our own codegen, ThinLTO). See chat: wrap-don't-decompose largely sidesteps the need.
