# Anneal — v1 Design Document

> Status: Design / pre-implementation
> Scope: Defines the Milestone 1 (Thesis MVP) feature set, the v1.x roadmap, architecture, deliberate deferrals, benchmark gates, and known risks.

---

## Table of Contents

1. [Motivation and Design Principles](#1-motivation-and-design-principles)
2. [Milestone Structure](#2-milestone-structure)
3. [System Architecture](#3-system-architecture)
4. [The BUILD Language](#4-the-build-language)
5. [Rules and the Rule Model](#5-rules-and-the-rule-model)
6. [Configurations and Transitions](#6-configurations-and-transitions)
7. [Execution Model and Hermeticity](#7-execution-model-and-hermeticity)
8. [Caching and the Correctness-Neutral Invariant](#8-caching-and-the-correctness-neutral-invariant)
9. [Remote Cache and Remote Execution](#9-remote-cache-and-remote-execution)
10. [Persistent Workers](#10-persistent-workers)
11. [Query System](#11-query-system)
12. [Test Infrastructure](#12-test-infrastructure)
13. [First-Party Rules](#13-first-party-rules)
14. [Generated Native Packages and Cross-Language Routing](#14-generated-native-packages-and-cross-language-routing)
15. [Migration and Adoption](#15-migration-and-adoption)
16. [Distribution and Bootstrap](#16-distribution-and-bootstrap)
17. [Errors and Diagnostics](#17-errors-and-diagnostics)
18. [CLI Reference](#18-cli-reference)
19. [Reference Schemas](#19-reference-schemas)
20. [Benchmark Gates](#20-benchmark-gates)
21. [Deferred Features and Known Limitations](#21-deferred-features-and-known-limitations)
22. [Open Questions and Risks](#22-open-questions-and-risks)

---

## 1. Motivation and Design Principles

### 1.1 What this exists to solve

Modern software is increasingly polyglot. A single product might combine a Rust core, a TypeScript frontend, and generated configuration or schema data. Each ecosystem has its own build tool (Cargo, pnpm) that is excellent within its boundary but says nothing about composition *across* boundaries.

Existing cross-language build systems (Bazel, Buck2) solve composition but impose significant costs: steep learning curves, poor migration stories, weak incremental-development ergonomics, and a tendency to require abandoning the native per-language tooling developers already know and that already contains sophisticated incremental engines.

Anneal provides cross-language build composition **without** abandoning native ecosystems. It wraps and orchestrates existing tools rather than replacing them.

### 1.2 Value proposition

> **Bazel-grade outer-loop orchestration without Bazel-grade ecosystem migration.**

More fully:

> Anneal provides a hermetic outer build graph for native Cargo and pnpm projects, preserving their lockfiles and incremental caches while adding cross-language generated-artifact routing, content-addressed caching, affected-test selection, and reproducible CI.

The distinctive bundle: existing tools like Cargo and pnpm remain the source of truth. Anneal wraps them as coarse package/workspace targets, snapshots their mutable caches as **correctness-neutral accelerators**, and routes generated artifacts into those tools as native package-local inputs.

### 1.3 Borrowed primitives vs. the differentiating combination

Anneal does not claim to invent the foundational ideas it uses. Its novelty is in the combination, not the parts.

| Borrowed / established | Differentiating combination |
|---|---|
| Content-addressed storage, action graph, Starlark, sandboxing, remote cache, graph query, persistent workers | Native-tool-preserving orchestration; the correctness-neutral stateful **snapshot protocol**; package-level ownership; **generated native packages** materialized into native ecosystems as ordinary inputs |

The strongest framing of this system is **not** "a new Bazel." It is: *a native-tool-preserving, package-level, hermetic outer build system for Rust/TypeScript monorepos, with correctness-neutral stateful cache snapshots and first-class generated-artifact routing.*

### 1.4 Core hypothesis and the central invariant

**Core hypothesis.** Native ecosystem tools already contain sophisticated incremental engines. A useful polyglot build system should *preserve* those engines, not replace them. Anneal's job is to make their inputs, outputs, environment, generated files, and cache state explicit enough to be reproducible and shareable.

**The central technical invariant (anchors the entire snapshot protocol):**

> **Restored native cache state must be correctness-neutral.** Providing a snapshot may make a build faster, but must never change the semantic output. A build that restores a warm `target/` and a build that starts cold must produce identical artifacts.

Every design decision around stateful caching is subordinate to this invariant. If a snapshot could change output, it is a bug, not an optimization.

### 1.5 Core principles

**Hermeticity is non-negotiable.** Build actions are pure functions of their declared inputs. Nothing from the host environment leaks into a build unless explicitly declared.

**Content-addressing everywhere.** Every artifact is identified by the hash of its content, so caching, deduplication, remote execution, and reproducibility verification fall out of one mechanism.

**Compose with ecosystems; don't replace them.** Cargo is the source of truth for Rust resolution; pnpm for the npm ecosystem. Anneal consumes their lockfiles and delegates inner-loop work to them, owning only the cross-cutting and cross-language layer.

**Native tools are opaque inner engines.** For a `cargo_workspace`, Anneal does **not** model every rustc invocation. Cargo remains an opaque inner scheduler; Anneal models coarse actions (build, check, test-compile, test-run, codegen, materialization). See [§3.2](#32-target-graph-vs-action-graph).

**Package-level ownership; exclusive ownership, shared dependency.** The unit of ownership is the package (a Cargo workspace, a pnpm workspace), matching how ecosystems already organize code. Ownership is **exclusive at the package level**: every path resolves to exactly one owning **package** — `owner(path)` is the nearest enclosing package (a filesystem walk, [§11.3](#113-first-class-affected-and-why-milestone-1)) — so a package directory has a single owning workspace target. A **generated** path is owned solely by its producer and may not collide with a source file or another generated output; a collision is a build error ([§14.4](#144-three-modes-of-native-tool-interop)). Targets *within* a package (a `filegroup`'s selection, a generator) freely reference package-owned files — that is shared *usage*, not competing ownership. And **dependency is shared**: any number of targets may *consume* a file by depending on the *owning target* (e.g. a `filegroup`) and receiving it through its provider, never by co-claiming the raw path. Exclusive package ownership keeps `owner(path)` total, so affected-target selection is sound; shared dependency keeps reuse ergonomic.

**System provides policy; rules provide mechanism.** Cross-cutting concerns (diagnostics, test-result aggregation, caching, configuration) are handled uniformly by the system. Rules describe how to invoke specific tools.

**Optimizations must be invisible.** Caching, workers, and remote execution never change build semantics (the §1.4 invariant generalized).

### 1.6 Relationship to Bazel and Buck2

Anneal adopts the action-graph model, content-addressed execution, and (via Starlark) the configuration language of Bazel/Buck2. It is **not** a drop-in replacement for either and makes no attempt at rule-ecosystem compatibility.

Deliberate divergences: surface simplicity over surface power (five fixed axes, three transitions, no custom transitions in v1); wrap-don't-replace for ecosystems (consume native lockfiles rather than reimplement resolution); package-level rather than file-level ownership; a correctness-neutral snapshot protocol for stateful caches; strict env hermeticity with sandbox-provided standard variables; and — critically — native tools modeled as opaque coarse engines rather than decomposed into fine-grained actions.

---

## 2. Milestone Structure

The work is split into two tiers. The first proves the thesis to ourselves; the second makes it compelling to adopters and adds operationally-heavy capabilities.

### 2.1 Milestone 1 — Thesis MVP

Goal: **internally prove the architecture works.** Not an adoption-facing release.

In scope:

- Starlark loading; package-level target graph
- Local CAS + action cache + materializer + sandbox
- Rules: `cargo_workspace`, `pnpm_workspace`, `nickel_eval`, plus `filegroup` / `alias` / `genrule`
- **Generated-native-package routing**, demonstrated cross-boundary via **Nickel → TypeScript**
- Conservative whole-cache snapshot save/restore (no deep semantic pruning)
- `affected` and `why` queries
- GitHub Actions cache integration
- A direct **platform-transition test** (validates the transition machinery without a user-facing rule that depends on it — see [§6.4](#64-the-three-transitions))
- Structured errors and the diagnostics channel
- The benchmark gates of [§20](#20-benchmark-gates)

### 2.2 v1.x — adoption and scale

Built on Milestone 1's proven foundations:

- `nextjs_app`
- **Rust → WASM → TypeScript** with wasm-bindgen-generated typed bindings (the cross-language *type-safety* demonstration and primary adoption asset)
- Remote cache backend, then remote execution
- Persistent TypeScript worker
- Resourceful tests (service provisioning), sharding, richer test infra
- Daemon / RPC interface
- Python (`uv_workspace`) and Go (`go_module`) rules

### 2.3 What Milestone 1 does *not* prove

Stated plainly to keep the benchmark-gate discipline honest:

- **Cross-language type safety is not proven.** Nickel → TS demonstrates generated-artifact *routing* across a boundary, not a typed contract that breaks on a producer-side change. Nickel emits data (JSON); it does not emit target-language type definitions. Genuine cross-language type safety arrives in v1.x via Rust → WASM → TS, where wasm-bindgen emits `.d.ts` as a native part of its operation. We do not simulate this in Milestone 1 by bolting a JSON-Schema-to-TS pipeline onto Nickel.
- **Remote execution is not proven** (deferred; remote cache comes first).
- **Performance at scale beyond the benchmark repos is not proven.**

What Milestone 1 *does* prove: native-tool-preserving orchestration, the correctness-neutral snapshot protocol, package-level coarse-action modeling, generated-native-package routing across an ecosystem boundary, composing incremental caches, affected-target selection, and reproducible CI — i.e., the thesis.

---

## 3. System Architecture

### 3.1 The three phases

```
BUILD files → (loading) → target graph → (analysis) → action graph → (execution) → outputs
```

**Loading** reads BUILD files, resolves `load()` statements, and produces the *target graph* (nodes are rule instances, edges are declared dependencies). Parallelized by package.

**Analysis** applies configurations and evaluates rule functions, expanding each target into actions. Output is the *action graph*. Parallelized by configured target and memoized incrementally.

**Execution** runs the action graph. Cached actions are skipped; the rest dispatch to an executor. Parallelized across independent actions.

### 3.2 Target graph vs. action graph

These are distinct, and for Anneal the distinction is smaller and shallower than in Bazel — deliberately.

The **target graph** is what users write: roughly one node per package (package-level ownership), so a medium monorepo has thousands, not tens of thousands, of nodes.

The **action graph** is what executes. **Crucially, for native-tool rules Anneal does not decompose the tool into fine-grained actions.** A `cargo_workspace` does not expand into one action per rustc invocation. Cargo remains an opaque inner scheduler; Anneal models a handful of coarse actions per workspace:

- `build` / `check`
- `test-compile` / `test-run` (per `(crate, test_type)`)
- `codegen` (where applicable)
- generated-package materialization

This is the honest abstraction: **Cargo owns the inner loop; Anneal owns the outer loop.** The action graph is therefore far smaller than a Bazel-style fully-decomposed graph — a direct consequence of preserving native engines rather than replacing them, and a feature rather than a limitation.

### 3.3 Configured targets

The formal unit of work is the **configured target**: a `(label, configuration)` pair, where `Configuration = (Platform, AxisValues)`. The same label built under different configurations (host vs. wasm32, debug vs. release) produces different action subgraphs and outputs. Analysis takes a configured target and produces actions; the action is the unit of execution.

### 3.4 CAS and materializer

All artifacts live in a content-addressed store. The **materializer** bridges the CAS and the filesystem:

1. Prepare a per-action sandbox root.
2. Materialize declared inputs from the CAS into the sandbox at expected paths (hardlink on Linux; copy-on-write clone on macOS).
3. Run the action; it reads materialized inputs and writes declared output paths.
4. Capture outputs, hash them, store in CAS, record `(action_id, output_name) → hash`.
5. Clean up the sandbox (or retain for debugging on request).

Materialization is O(1) per file with negligible disk. On **Linux**, inputs are hardlinked from the CAS (shared inodes), with strict read-only enforced by the sandbox's read-only bind mounts ([§7.3](#73-sandboxing)). On **macOS**, inputs are APFS copy-on-write clones (`clonefile`) marked read-only: a write through a materialized input copies-on-write and **cannot corrupt the store** (and the per-inode hardlink limit is sidestepped). CAS and sandboxes share one filesystem; cross-filesystem materialization falls back to copy (avoided by configuration).

---

## 4. The BUILD Language

### 4.1 Starlark, via starlark-rust

BUILD files are written in **Starlark**, evaluated by `starlark-rust`. Starlark is pure, deterministic, proven at scale, and immediately familiar to anyone from Bazel/Buck2, with mature tooling.

The differentiating value of Anneal is architectural, not linguistic. Building a custom DSL or adopting a niche language would consume effort better spent on the actual differentiators and would underwrite an ecosystem we do not control. The "looks like Python but isn't" criticism is a one-time learning cost and primarily a documentation problem.

### 4.2 Restricted user-facing subset

Users interact with a small subset: rule invocations, literals, variable bindings, simple expressions, `load()`, and ternary conditionals. Not promoted in user docs (though available to rule authors): `def`, loops, `if/elif/else` blocks, mutation, most of the standard library. A linter flags non-recommended patterns in user BUILD files.

### 4.3 Schema validation at rule boundaries

Starlark has no static types; Anneal validates rule arguments against declared schemas at the rule boundary, at load time, producing errors that point at the user's BUILD file before analysis proceeds.

### 4.4 File conventions

- `BUILD` (or `BUILD.starlark`) — per-package build files, one per package directory.
- `*.bzl` — shared Starlark libraries, imported via `load()`.
- `WORKSPACE` — at the workspace root; marks the root and holds workspace-level configuration.

First-party rules are auto-loaded. `load()` is reserved primarily for future user-defined rules.

---

## 5. Rules and the Rule Model

### 5.1 First-party rules only in Milestone 1

Rules are not publicly extensible in Milestone 1. The set is fixed and shipped as a standard library. Deferring a public rule API lets us discover the right primitive set through real first-party development rather than designing it in the abstract, keeps the system/rule boundary clean, and lets us evolve internals freely.

First-party rules are themselves implemented against an **internal rule API**. This API may be exposed *experimentally* for early adopters who need an unsupported language — but it is explicitly unstable, undocumented as a contract, and subject to change without notice. We are cautious here: an "experimental" API that adopters build on can become a de facto contract, which is exactly what deferring extensibility is meant to avoid. Experimentation is permitted; reliance is discouraged.

### 5.2 Rules produce actions

A rule maps a target's declared attributes to a set of (coarse) actions and produces/consumes typed *providers* along dependency edges (FileSet, TestSuite, ToolchainInfo, LibraryInfo, BinaryInfo, Diagnostic). For the rule model developed from first principles — the analyze-phase contract, the eight obligations, the inference↔declaration spectrum, and the hermeticity-vs-determinism line that governs cacheability — see `docs/rules.md`.

### 5.3 The system/rule boundary

On the **system** side: diagnostics presentation, error formatting, progress display, cache and correctness invariants, cross-cutting aggregation, worker/RE management, configuration resolution. On the **rule** side: how to invoke a tool, the tool's inputs/outputs/providers, and translation of tool output into the system's structured formats. This division is maintained by discipline now, while rules are first-party, so the boundary is established before any public API exists.

### 5.4 Dependency information flow

Information crosses a dependency edge on exactly two channels:

- **Providers flow up** (dependency → dependent): a dependency exposes typed providers; the dependent reads them and adapts. This is the dominant flow.
- **Configuration flows down** (the dependent's context → dependency), and only at explicit transitions ([§6.4](#64-the-three-transitions)): the sole sanctioned way for *what a dependent needs* to change *how a dependency is built*.

**A configured target's output is a pure function of `(label, configuration)` — never of which targets depend on it.** There is no third channel of ad-hoc per-edge parameters: a dependent cannot pass arbitrary values down to reconfigure a dependency, because that would make the dependency's output depend on its consumers and destroy the content-addressed identity that caching, deduplication, and remote sharing rest on ([§1.5](#15-core-principles), [§8](#8-caching-and-the-correctness-neutral-invariant)).

So "wanting a variant of a dependency" is expressed by **depending on the target (or configuration) that produces it** — through graph topology and instantiation-time attributes, not a runtime request. Surface syntax that *looks* like edge parameterization (e.g. depend on `//x` "as JSON") is admissible only as sugar, and only when it desugars to a cache-safe form: **selecting** among outputs the dependency already offers, or depending on a **distinct configured variant** whose distinguishing parameter is part of the cache key. The dividing line is whether the parameter enters the dependency's identity (safe) or silently alters its output without doing so (forbidden). This mirrors Bazel and Buck2, which channel all downward influence through configuration and transitions and provide no per-edge parameter passing.

### 5.5 Providers: outputs and metadata

A target's providers carry information on four separate channels, kept apart so none degrades into an untyped grab-bag:

1. **Typed metadata providers.** Structured facts *about* a target that a dependent needs but that are not files — a library's crate name and features, a toolchain's location and version, the inputs required to link against a target. Typed (`LibraryInfo`, `ToolchainInfo`, …) and read by field. **Metadata never lives in a file group.**
2. **File outputs as a default group plus named groups.** A target's *file* outputs are a **default group** (what a dependent receives by depending on the target) plus zero or more **named groups** (selectable subsets or variants), each a `FileSet` of content-addressed artifacts. The set of named-group keys a rule offers is part of its **declared, validated interface** — not free-form strings — so selecting an unknown group is an analysis error and the menu is discoverable. This is a deliberate divergence from Bazel's `OutputGroupInfo`, whose open string keys accrete into a junk drawer of magic names.
3. **The diagnostics channel** ([§17.2](#172-diagnostics-channel)). Observations a rule makes — lints, validation findings, deprecations, unused-input detection — flow as structured `Diagnostic` values, not as files in a group.
4. **Configuration** ([§6](#6-configurations-and-transitions)). Outputs that exist only under a configuration (coverage data, sanitizer artifacts) are gated by the relevant axis, not exposed as permanent groups.

Keeping these four apart is the deep-module discipline applied to providers: each is a narrow, typed channel that means exactly one thing, rather than one wide untyped map absorbing every concern.

### 5.6 Variant menus and selection

A producer often offers a *menu* of file variants — a config rendered as JSON/TOML/YAML, a schema compiled to several languages, the per-`(crate, test_type)` results of a workspace. The menu is the named output groups of [§5.5](#55-providers-outputs-and-metadata), governed by three rules:

- **The rule computes the validated menu.** Its keys may come from a fixed *capability* (Nickel exports to a known set of formats), from *instance introspection* (a Cargo workspace's test types are those each crate actually has), or from an explicit attribute. An optional instance attribute may **narrow** the menu as policy (e.g. "this config is only consumed as TOML").
- **Selection, never parameterization.** A dependent **selects** an entry from the menu the producer already determined; the requested key never flows into the producer to change what it builds. This is enforced *structurally* — a producer is analyzed without knowledge of its dependents ([§5.4](#54-dependency-information-flow)) — so the menu is fixed by `(label, configuration, attrs)`, and a selected variant is a content-addressed node keyed in part by the variant. "Implicit support for any variant" is therefore realized as the menu **defaulting to the rule's full capability**, never as a parameter passed down an edge.
- **Granularity is a modeling choice, not a second mechanism.** Whether variants are separate targets (one per variant) or one target offering a menu is a choice over the *same* primitive: separate targets populate only their default group; a menu target populates named groups. Both select cache-safely.

Building only the variants a build actually consumes requires **demand-driven output pruning** ([§21](#21-deferred-features-and-known-limitations)): the build's intent selects which groups of the requested targets are roots, and an action runs only if its output is reachable from a demanded group. Until that exists, a multi-variant producer's unconsumed variants are still built, so the near-term idiom is one target per variant (demand-driven by the dependency graph itself). Variant sets whose membership is known only after a tool runs need **tree artifacts** ([§21](#21-deferred-features-and-known-limitations)), the companion to the static menu.

---

## 6. Configurations and Transitions

### 6.1 Platforms as the dominant concept

The primary thing a user picks is the **target platform** (constraints + target triple). Most users never think beyond this.

### 6.2 The five universal axes

| Axis | Values |
|------|--------|
| `opt_level` | `debug`, `release`, `release_with_debuginfo` |
| `lto` | `off`, `thin`, `full` |
| `debug_info` | `none`, `line_tables_only`, `full` |
| `sanitizer` | `none`, `address`, `thread`, `memory`, `undefined` |
| `coverage` | `on`, `off` |

Axes are universal in name and type; each rule declares which it consumes. **Axes a rule does not consume are excluded from its cache keys** (automatic cache trimming). No user-defined axes in v1.

### 6.3 Per-rule interpretation

Users express intent (`--opt-level=release`); each rule translates it (see [§13.6](#136-axis-interpretation-matrix)). Rules consuming no axes (e.g., `nickel_eval`) produce configuration-invariant output and become cache-sharing points across all configurations.

### 6.4 The three transitions

- **`host_to_exec`** (automatic): an action's exec configuration derives from the worker platform.
- **`target_to_exec`** (default for tool dependencies): a tool executed during a build runs under an exec configuration.
- **explicit platform transition**: building a subtree for a different platform (e.g., Rust for wasm32). **No user-facing Milestone 1 rule exercises this**, since Rust→WASM is deferred — so Milestone 1 includes a **direct platform-transition test**: build a bare Rust crate for two target platforms and assert distinct, correctly-cached configured targets and correct outputs. This prevents shipping untested configuration machinery.

No custom transitions in v1.

### 6.5 Within-package conditional compilation is the language's job

`#[cfg(...)]`, build tags, etc. are handled by the compiler, not the BUILD layer.

### 6.6 Specifying configuration

CLI flags only in v1 (`--target`, `--opt-level`, `--lto`, `--sanitizer`, `--debug-info`, `--coverage`); unspecified axes fall back to host defaults. Build-mode files and per-target defaults are deferred (non-breaking additions). The internal config data model is structured from day one to support `explain` and future build modes.

---

## 7. Execution Model and Hermeticity

### 7.1 Unified executor abstraction

Local and remote execution share one interface; `LocalExecutor` and `RemoteExecutor` are interchangeable and produce identical `ActionResult` shapes. This prevents "works locally, fails remotely" divergence. The design is **remote-first internally, local-first in user experience** — though note that in Milestone 1 only local execution exists ([§9](#9-remote-cache-and-remote-execution)).

### 7.2 Execution modes

Actions declare a mode: **`sealed`** (hermetic, strict input isolation; default for cacheable actions), **`permeable`** (relaxed isolation for actions needing access beyond declared inputs; not cacheable), **`native`** (direct execution, used by `mybuild exec`).

### 7.3 Sandboxing

- **Linux**: mount namespaces with read-only bind mounts of declared inputs (hardlinked from CAS) for `sealed` mode. Kernel-enforced.
- **macOS**: `sandbox-exec` profiles. Best-effort (~95% in practice), not strict — see [§22](#22-open-questions-and-risks). Optional Linux-VM mode for strict needs.
- **No Windows in v1.**

The materializer is shared across platforms; only the isolation layer differs. See [`docs/sandboxing.md`](docs/sandboxing.md) for the full materialization/isolation model, the per-platform hermeticity guarantees, and the use of read-tracking to **enforce** declared inputs (catch under-declaration) rather than to relax invalidation.

### 7.4 Strict environment hermeticity

Actions see only the environment the system provides. The sandbox sets canonical, deterministic values for standard variables (`PATH`, `HOME`, `USER`, `LANG`/`LC_*` = `C.UTF-8`, `TMPDIR`, `TZ` = `UTC`, `HOSTNAME`, `TERM` = `dumb`, `SHELL`, `PWD`). Actions declare any additional variables via the `env` field; both names and values enter the cache key. **There is no host-environment passthrough mechanism.**

### 7.5 Secrets and private registries — deliberately out of scope

Milestone 1 is **explicitly scoped to public-dependency workflows.** Because of strict env hermeticity and no passthrough, workflows requiring host credentials (private Cargo registries, private npm packages, git deps needing auth) are **not supported.**

This is a deliberate rejection, not an oversight. We considered adding "minimal declared secrets" and rejected it because a half-built secrets mechanism contaminates the design in ways that are expensive to undo:

- A secret value must never enter a cache key (or it leaks via cache identifiers); retrofitting that exclusion onto an ad-hoc mechanism is error-prone.
- Scrubbing secrets from logs, the event stream, and diagnostics is a cross-cutting concern best designed once, deliberately.
- Per-action sandbox exposure of secrets is a real design surface, not a flag.

A proper first-class secrets concept (declaration, cache-key exclusion, scrubbing, per-action sandboxing, provenance) is planned post-Milestone-1. Until then, Anneal targets open-source and public-dependency repositories. This is an honest limitation stated plainly rather than a rushed mechanism that compromises the hermeticity that the whole system rests on.

### 7.6 The `mybuild exec` escape hatch

For commands not natively wrapped, `mybuild exec <command>` runs in a sandbox. Default mode is permissive (workspace inputs available, network allowed, scrubbed environment), **non-cacheable**, and **not part of the action graph**. `--hermetic --inputs=... --no-network` opts into rule-like strictness; `--explain` shows the sandbox configuration. Common-subcommand wrappers (`mybuild cargo expand`, etc.) are deferred — purely additive.

---

## 8. Caching and the Correctness-Neutral Invariant

### 8.1 Content-addressed action cache

The action cache maps an **action digest** to a **result digest**; a hit skips execution and fetches outputs from the CAS. The digest is computed from: command, declared input content hashes, the `env` map (keys and values), working directory, execution mode, cache policy, worker name, relevant platform requirements, and **only the configuration axes the rule consumes** (trimming), plus the sandbox version. Excluded: timestamps, action name, host environment.

### 8.2 The snapshot protocol for stateful caches

Native tools maintain incremental on-disk state that is not a clean artifact: Cargo's `target/`, pnpm's store, `node_modules`, `.next/cache/`, `.tsbuildinfo`. Forcing these into the atomic-derivation model either wastes incrementality or breaks hermeticity.

Anneal models these as **snapshots**, governed entirely by the [§1.4](#14-core-hypothesis-and-the-central-invariant) invariant: **restoring a snapshot may make a build faster but must never change its semantic output.** A snapshot is an accelerator, never an input that affects results.

Three rule-author operations:

- **`save`**: produce a content-addressed snapshot of the cache directory after a build.
- **`restore`**: restore a snapshot into a fresh workdir before a build (cold start handled gracefully).
- **`prune`**: reduce a snapshot's size (see scope limits below).

Snapshots are content-addressed in the same CAS, keyed at a **coarser** granularity than action cache keys — e.g., `(toolchain, lockfile, target_triple, profile)` — coarse enough that small source edits hit the same snapshot, fine enough that a toolchain bump invalidates it. Eviction policy (LRU, size/age caps) belongs to the system; the rule declares only *what* to prune.

### 8.3 Pruning scope (v1: conservative)

`prune` is intentionally modest in v1. **v1 snapshots are conservative whole-cache snapshots.** Supported pruning is **safe and coarse**: deleting whole stale snapshot generations, or removing unreachable entries from a content-addressed store (e.g., pnpm store entries with no referencing lockfile).

Deep tool-internal semantic pruning — walking Cargo's `target/` to remove entries keyed to crate versions, feature sets, or rustc fingerprints not in the current closure — is **experimental and explicitly not promised for v1.** It is an aspiration that motivates the protocol's shape, not a v1 deliverable. We do not promise deep `target/` pruning until it is proven safe under the correctness-neutral invariant.

### 8.4 Local and remote caching

Content-addressed identity is shared across machines: an artifact built locally hits cache for a build of the same inputs elsewhere. Cross-PR and cross-machine sharing fall out because cache identity is branch- and machine-independent. (Milestone 1 ships the local CAS and the GitHub Actions integration; a standalone remote cache *backend* is v1.x — see [§9](#9-remote-cache-and-remote-execution).)

### 8.5 CI cache integration

A co-shipped GitHub Action wraps the cache protocol: **restore** relevant entries (keyed on content-addressed input hashes, more precise than heuristic file hashing) before the build; **save** new entries incrementally after; **save on success only**. Because keys are precise, edits invalidate only what they affect and cross-PR sharing works automatically. The backend defaults to the GitHub Actions cache and is pluggable. The Action is deliberately "dumb" (fetch/build/push) with all intelligence in the build system.

### 8.6 Provenance and `cache push`

Local CAS entries are tagged with creation timestamp and producing action digest, enabling `mybuild cache push` (upload local entries to a remote CAS — useful after offline work). Ships in v1 as a minimal "upload everything not already remote."

---

## 9. Remote Cache and Remote Execution

### 9.1 Sequencing: remote cache first, remote execution later

This is a deliberate change from earlier thinking. **Remote execution is deferred out of Milestone 1 entirely.** It is a large implementation and operational burden that proves nothing about the thesis — the thesis is about native-tool preservation, snapshots, and routing, none of which require distributing execution across a worker fleet.

Sequencing:

1. **Milestone 1**: local CAS + action cache + GitHub Actions cache integration. No remote backend, no RE.
2. **v1.x**: a standalone **remote cache backend** (CAS + action cache over the network, no execution) — this delivers most of RE's practical value (warm caches shared across a team and CI) at a fraction of the cost.
3. **Later v1.x / v2**: **remote execution**, if and when benchmarks and demand justify the operational burden.

### 9.2 Protocol posture

Earlier drafts argued for a custom RE protocol because REAPI maps poorly onto our snapshot protocol, diagnostics channel, and persistent workers. That argument is sound *if building RE* — but it is subsumed by the decision not to build RE in the first milestone at all.

Revised posture: **prefer REAPI-compatible foundations** for the remote cache, and extend only where the snapshot protocol demonstrably requires it. A custom protocol is not adopted speculatively. If RE is eventually built, the protocol question is revisited then, with the benefit of a working remote cache and real usage data.

### 9.3 When RE arrives (v1.x design notes, not Milestone 1)

For reference, the eventual RE design: a unified executor abstraction (already in place — [§7.1](#71-unified-executor-abstraction)); lazy materialization of remote outputs (download only what's needed); worker selection via the universal axes (platform as hard requirement, warm toolchain as soft preference, content-addressed toolchains so any worker can serve any action); and offline/failure handling via per-action local-fallback with a circuit breaker. None of this is Milestone 1 scope.

---

## 10. Persistent Workers

### 10.1 Concept (mostly v1.x)

A persistent worker is a long-lived process wrapping a build tool, amortizing the tool's **in-memory startup cost** across many actions — the complement to the sandbox (*workers amortize in-memory state; sandboxes amortize filesystem state*). It helps only tools with expensive startup relative to per-invocation work (TypeScript's `tsc`, JVM compilers). It does **not** help Cargo, which already amortizes internally across a single `cargo build`.

### 10.2 Milestone 1 scope

**No persistent workers in Milestone 1.** Cargo doesn't benefit; the TypeScript worker — the highest-impact case — is **v1.x**. Milestone 1 runs all actions as subprocesses.

### 10.3 Design (for the v1.x TypeScript worker)

Workers are **per-action, not per-rule**: an action declares worker eligibility and which worker it needs; pools are shared across all actions requesting the same worker. Each worker is a **shim** we ship that speaks the worker protocol and wraps a specific tool, constraining the tool's file access to declared inputs (e.g., a custom `CompilerHost` for `tsc`) to preserve hermeticity despite the long-lived process. Workers are an optimization: the same action via a worker and via a subprocess must produce identical outputs, verified by determinism tests, state-contamination tests, mutation tests, and production sampling. Escape valves: `--no-workers`, per-worker disable, periodic restart, verification mode.

---

## 11. Query System

### 11.1 Two query layers

- **Target queries** (`mybuild query`): operate on the target graph; configuration optional.
- **Action queries** (`mybuild aquery`): operate on the action graph; require analysis.

Configuration is an optional parameter on target queries rather than a separate command, justified by our simpler configuration model.

### 11.2 Query language and operators

Bazel-style expression language for the CLI (`deps`, `rdeps`, `kind`, `attr`, `owner`, `somepath`, `testsof`; action-level `actions`, `inputs`, `outputs`, `consumers`, `producers`). A programmatic Starlark graph API serves tooling. Custom query functions deferred.

### 11.3 First-class `affected` and `why` (Milestone 1)

- **`affected --since=<commit>`** — the test/build targets affected by a diff. The primary CI primitive. Package-level ownership makes `owner(path)` a filesystem lookup, not a graph traversal.
- **`why <target>`** — explains a rebuild (which input changed, with hashes, and the dependency chain) or a target's inputs, backed by per-action provenance.

Related: `explain` (effective configuration/metadata), `audit configurations` (enumerate configurations in use). The daemon/RPC interface for long-running tool queries is v1.x.

---

## 12. Test Infrastructure

### 12.1 Tests as a distinct concept

Tests have structured results, allowed flakiness, sharding, isolation needs, and selective execution, getting dedicated machinery while composing with the action/cache/configuration systems. (Milestone 1 ships the core; sharding, flakiness retries, and resourceful tests are v1.x — see [§12.5](#125-v1x-test-features).)

### 12.2 Granularity: per-(crate, test_type)

For a Cargo workspace, the rule generates one test target per `(crate, test_type)` — unit, integration, doc — only for types a crate has:

```
//workspace:crate_a_test_unit         → cargo test --package crate_a --lib
//workspace:crate_a_test_integration  → cargo test --package crate_a --tests
//workspace:crate_a_test_doc          → cargo test --package crate_a --doc
```

This gives per-type cache granularity, per-type resource declarations, and per-type parallelism, mapping directly to Cargo flags. Granularity stops at the test-type level; individual functions are addressed via framework filters (`--filter`). Convenience: `mybuild test //workspace:crate_a` and `mybuild test //workspace/...` expand appropriately.

The trade is a modest cold-cache/workspace-wide-change overhead (~5–20%, mitigated by the shared `target/` snapshot) for major incremental and selection wins. A batch-invocation optimization (single `cargo test --workspace` when many targets are requested at once) is available later if benchmarks justify it.

### 12.3 Compile/run split

Each `(crate, test_type)` produces a cacheable, content-addressed **compile** action (`cargo test ... --no-run --message-format=json`, binary paths parsed from JSON) and a **run** action depending on it. A source change recompiling to a byte-identical binary causes a compile cache miss but a run cache **hit** — tests don't re-run unnecessarily. (Doc tests are a partial exception; `cargo test --doc` does not produce reusable binaries and may run combined.)

### 12.4 Structured results

Test runners emit a structured result protocol ([§19.2](#192-test-result)) — per-case outcomes, durations, failures, retry counts, shard info — translated from each framework's native output. Case identity (`target label + test name`) is stable for external history tooling.

### 12.5 v1.x test features

Deferred to v1.x: **sharding** (`shard_count`, results merged), **flakiness retries** (opt-in `flaky = true`, default no retries, distinct `passed_after_retry` outcome, retry rates to the event stream), **isolation primitives** (port allocation, declarable resources), and **resource provisioning** (per-test ephemeral services like Postgres via shipped provider shims, or external-declared). Milestone 1 ships deterministic test execution with structured results and the compile/run split.

### 12.6 Caching and failure inspection

Cacheable per-target (default on for unit-test-shaped, off for resource-dependent tests). Under lazy materialization (v1.x, with RE), failed-test artifacts stay remote until requested via `mybuild test --inspect`. In Milestone 1 (local only) artifacts are already on disk.

---

## 13. First-Party Rules

### 13.1 Milestone 1 rule set

| Rule | Purpose |
|------|---------|
| `cargo_workspace` | Wraps a Cargo workspace; coarse build/check/test actions; conservative snapshot of `target/`. |
| `pnpm_workspace` | Wraps a pnpm workspace; node_modules materialization; consumes generated native packages. |
| `nickel_eval` | Evaluates a Nickel file to produce an output (JSON). Pure; consumes no axes. |
| `filegroup` | Groups files into a target for use as inputs. |
| `alias` | Alternate name for another target. |
| `genrule` | Generic "run command, produce outputs" escape hatch for one-off codegen. |

`pnpm_workspace` is required in Milestone 1, not optional: without it there is no cross-language boundary to route across, which is the thesis. Next.js (`nextjs_app`) and the Rust→WASM path are **v1.x** ([§13.5](#135-v1x-rules)).

### 13.2 Rationale for this set

Two native ecosystems (compiled Rust, npm-world TypeScript) plus a pure generator (Nickel) exercise the architecture across fundamentally different tools and demonstrate generated-native-package routing across an ecosystem boundary (Nickel → TS). `cargo_workspace` also enables self-hosting.

### 13.3 Per-rule completeness

Each native rule supports building primary artifacts, idiomatic test execution, structured results, cache integration (action + conservative snapshot), the configuration axes (per-rule interpretation), and sandboxed execution. Each does **not** wrap every tool workflow (`cargo publish` is not wrapped — use `mybuild exec`) or every edge case (complex Cargo feature unification may be imperfect initially).

### 13.4 `cargo_workspace` mechanics

The rule introspects `Cargo.toml` and each crate's structure, generating per-`(crate, test_type)` test targets and coarse build actions. **Cargo is an opaque inner engine** — Anneal does not model rustc invocations. The build system is itself a Cargo workspace, so `cargo build` works directly, though direct Cargo use is not a first-class supported workflow (see [§14.4](#144-three-modes-of-native-tool-interop)).

`pnpm_workspace` is the harder rule — pnpm is a package-manager + script-runner, not a toolchain, so the rule splits into an inferred install layer and a declared script layer. Its full Milestone-1 scope and design reasoning (the `file:` name-resolved routing, the explicit `kind`, and the non-cacheable-by-default / sealed-and-reproducibility-gated cacheability model) live in `docs/pnpm-workspace.md`.

### 13.5 v1.x rules

`nextjs_app` (with explicit cacheability modes — [§14.5](#145-nextjs-cacheability-modes)); `rust_wasm_lib` (Rust → WASM → TS with wasm-bindgen typed bindings, the cross-language type-safety demonstration); `uv_workspace` (Python); `go_module` (Go). Deferred entirely: C/C++/JVM rules, framework rules beyond Next.js, npm/yarn (pnpm preferred for lockfile/workspace semantics), Poetry/pip-tools (uv preferred).

### 13.6 Axis interpretation matrix

| Rule | opt_level | lto | debug_info | sanitizer | coverage |
|------|-----------|-----|------------|-----------|----------|
| `cargo_workspace` | profile (`--release`) | `RUSTFLAGS` | `RUSTFLAGS` | `-Z sanitizer=` | `-C instrument-coverage` |
| `pnpm_workspace` | minification | ignored | source maps | ignored | test coverage |
| `nickel_eval` | ignored | ignored | ignored | ignored | ignored |
| `genrule` | env var to command | env var | env var | env var | env var |
| `filegroup`, `alias` | n/a | n/a | n/a | n/a | n/a |

Consumed axes drive cache trimming; `nickel_eval` consuming none makes its output shareable across all configurations.

---

## 14. Generated Native Packages and Cross-Language Routing

### 14.1 The "generated native package" concept

A **generated native package** is a generated artifact tree shaped so that a native tool (Cargo, pnpm, and later Next.js) consumes it as an **ordinary dependency or input** — not as a Bazel-style fine-grained target. This is a first-class differentiator and is introduced here as a named concept rather than buried in any single rule.

The general shape:

```
Nickel / Proto / OpenAPI / (later) Rust-WASM
   → generated JSON config / TS types / Rust crate / pnpm package
   → consumed by a native workspace rule as a package-local input
```

The value: **generated artifacts are routed into native ecosystem tools as if they were ordinary package-local files, without forcing the repo to adopt fine-grained target modeling.**

### 14.2 Storage and materialization

Generated artifacts are content-addressed in the CAS and materialized to conventional paths under `.mybuild/gen/`. The source tree is not polluted; `.gitignore` has a single entry (`.mybuild/`). Because outputs are content-addressed, a generator whose output is semantically unchanged does not invalidate consumers; diamond dependencies share one generation.

### 14.3 Milestone 1 cross-language demonstration: Nickel → TypeScript

A `nickel_eval` target produces a JSON artifact shaped as a generated native package (with a `package.json`); a `pnpm_workspace` consumes it as a workspace dependency, imported as an ordinary module. This exercises generated-native-package routing across an ecosystem boundary with composing caches: changing the Nickel source regenerates and rebuilds the consumer; changing only the consumer leaves the generator cached.

**This proves routing (and caching across the boundary), not type safety** — Nickel emits data, not target-language types. See [§2.3](#23-what-milestone-1-does-not-prove). The typed cross-language story (Rust → WASM → TS, where wasm-bindgen emits `.d.ts` natively) is v1.x and builds on exactly this routing foundation.

### 14.4 Three modes of native-tool interop

To resolve the apparent tension between "native tools keep working" and "raw `cargo build` isn't guaranteed":

- **Pure native mode**: existing `cargo build`, `pnpm build` work unchanged — for projects with no generated inputs and no reliance on sandbox-controlled env.
- **Materialized generated mode**: `mybuild materialize` writes generated native packages/files to stable paths so IDEs and native tools see them; native tooling then works against the materialized tree.
- **Fully mediated mode**: builds relying on sandboxed env or generated inputs go through `mybuild`.

This makes the tradeoff explicit rather than contradictory. If a file exists at a generator-managed path that the system did not generate, the build **fails with a clear error** rather than silently overwriting (`--force` overrides; `mybuild adopt-paths` reconciles).

### 14.5 Next.js cacheability modes (v1.x)

When `nextjs_app` ships, Next.js builds are **not** uniformly cacheable. Three modes prevent `.next/cache` from becoming a correctness foot-gun:

- **sealed**: no network, declared env only — cacheable.
- **declared-network**: explicit data/env/secrets — cacheable by policy.
- **permeable**: normal Next build — non-cacheable or local-only cache.

`.next/cache/` (incremental state, snapshot-managed) is distinct from `.next/` (build output, content-addressed).

### 14.6 When a generated artifact can be an in-graph dependency

A single question governs how a generated file can be consumed:

> **Who needs the generated content, and when — does *only the inner tool* need it at execution, or does *Anneal's own analysis* need it to shape the build graph?**

The answer sorts cases into three levels of increasing cost. The first two need no change to the build model; the third is a deliberate, deferred escape hatch.

**Level 1 — clean in-graph edge (no model change).** The generated file is content the *inner tool* reads at execution; Anneal never inspects it. It flows across the dependency edge as a content-addressed `Output`, materialized into the consumer. Caching and snapshots work normally — the artifact's digest is in the consumer's cache key, and the native tool's own dependency tracking (e.g. rustc's `include_str!` depinfo) keeps incremental builds correct. *Examples:* a JSON a Rust crate `include_str!`s; a generated `.rs` compiled by Cargo; a generated config routed into a pnpm workspace by relative path (plain-path — see `docs/pnpm-workspace.md` §4).

**Level 2 — materialized-generated staged pass (no engine change, but a prior pass).** Anneal's *analysis* still never reads the generated content, but a *checked-in manifest the build depends on* must reflect it — so that manifest is regenerated against the materialized artifact in a prior pass, then the build runs against the now-real files (the **materialized generated mode**, [§14.4](#144-three-modes-of-native-tool-interop)). This is *not* a resolution-model change: each analysis stays pure; the pipeline merely runs in stages. *Examples:* a generated package whose *own dependency set* must be hoisted into `pnpm-lock.yaml` (regenerate the lockfile first); a generated `Cargo.toml` handled via an explicit `mybuild materialize` step.

**Level 3 — deferred/dynamic analysis (a resolution-model change).** Anneal's *analysis itself* must read the generated content to shape the action graph — e.g. a generated `Cargo.toml` whose `members` determine which per-crate actions even exist, parsed *automatically* rather than via an explicit materialize step. Because analysis now depends on execution outputs, this collapses the analysis→execution phase separation and de-purifies analysis caching, demanding a DICE-style incremental engine. This is the heavyweight mechanism "wrap, don't decompose" was specifically chosen to avoid; it is deliberately out of scope (deferred indefinitely, possibly permanently).

The dividing line between Levels 1–2 and Level 3 is exactly the loading→analysis→execution ordering: a dependency edge (Level 1), or a staged pass that regenerates a manifest (Level 2), can carry anything the *execution* phase consumes — but only deferred analysis (Level 3) can serve content the *analysis* phase must read. Most cross-language routing lands at Level 1; a generated artifact's *own transitive dependency set* can pull it to Level 2; only a generated artifact that *structurally shapes Anneal's action graph* reaches Level 3 — and "wrap, don't decompose" keeps that case rare.

---

## 15. Migration and Adoption

### 15.1 Adoption is a gradient

Users move through stages: install alongside existing tooling (zero risk) → try one feature (the wedge) → expand → rely on as primary. The **wedge is CI caching** for an existing Cargo or pnpm workspace: a single config addition yields measurable speedup with no code changes.

### 15.2 Onboarding

- `mybuild init` — interactive setup.
- `mybuild init --detect` — scans for `Cargo.toml`, `pnpm-workspace.yaml`, etc., and scaffolds configuration without touching existing files.

### 15.3 Coexistence

Anneal owns `.mybuild/`; it does not claim native tool directories. The three interop modes ([§14.4](#144-three-modes-of-native-tool-interop)) make coexistence explicit. Lockfile-as-IR and package-level ownership mean users don't restructure code or mirror dependency graphs into BUILD files.

### 15.4 Deferred migration tooling

Bazel/Buck2 conversion tooling is deferred — the migrating population is small and demanding, and designing conversion before our destination model is stable is wasted effort. v1 targets greenfield and Cargo/pnpm-native projects.

---

## 16. Distribution and Bootstrap

### 16.1 Bootstrap

Anneal is a Cargo workspace; there is no special bootstrap problem. The very first build ever uses `cargo build` (no prior version exists). Thereafter, each release is built by the previous release; active development uses Anneal itself, with `cargo build` always available as a fallback (we wrap, not replace, Cargo). Releases use a **double-bootstrap**: build the new version with the previous (stage 1), rebuild with the new (stage 2), verify bit-for-bit identical (modulo version strings), ship stage 2 — catching non-determinism, confirming self-hosting, and verifying reproducibility across the boundary. Periodic full bootstraps from Cargo re-anchor against drift. CI exercises self-hosting on every PR.

### 16.2 Distribution

- **Primary**: pre-built binaries (linux-x86_64/arm64, macos-x86_64/arm64) via an installer script with checksum verification.
- **First-class for sophisticated users**: a Nix flake with a binary cache; `nix develop` provides a complete contributor environment. nixpkgs submission post-v1.
- **CI**: container images.
- **Source builds**: documented, not promoted.
- **No Windows** in v1.

Registry note: the bare `anneal` names on crates.io and npm are taken by unrelated projects, but this does not affect distribution (binary/Nix/container channels). Published crates, if any, use `anneal-*` names; the GitHub org and an `anneal.build`/`anneal.dev` domain are the real estate that matters.

### 16.3 Versioning

Semantic versioning post-1.0 (major = breaking user-visible API; minor = additive; patch = fixes). A `.mybuild-version` launcher convention (rustup-style) is planned; v1 may ship without the launcher but the CLI is structured so adding it is non-breaking. Long-term support for major versions.

---

## 17. Errors and Diagnostics

### 17.1 Structured errors

Errors are structured values, not strings: stable code (`MB0023`), category, source location pointing at user-written content (not internal rule library code), causal chain, short message, and doc-linked long-form explanation (`mybuild explain MB0023`). Source locations are preserved through rule evaluation. Multiple independent errors report together; causally-related ones may collapse (with `--verbose` to expand). CLI display is Rust-compiler-style. The structured-error architecture is a commitment; message prose iterates indefinitely.

### 17.2 Diagnostics channel

Rules emit structured `Diagnostic` values ([§19.3](#193-diagnostic)) as a secondary output channel (unused-file detection, lint findings, deprecations, type errors). The system aggregates, deduplicates, and presents uniformly across languages. Diagnostics are distinct from errors (an action may emit many diagnostics and zero-or-one error). Severity (`error`/`warning`/`info`/`hint`) governs display and halting; codes are namespaced or passed through from upstream tools; categories enable filtering. This is a deliberately lighter mechanism than Bazel aspects, covering the dominant "rule reports an observation" case; a graph-traversal primitive is deferred. Aggregation/suppression UX is deferred; the schema supports it.

---

## 18. CLI Reference

### 18.1 Commands

**Build/execute**: `build`, `run`, `test`, `check`.
**Query/inspect**: `query`, `aquery`, `why`, `explain`, `affected --since=<commit>`, `audit configurations`.
**Materialize/interop**: `materialize <targets> [--to=<path>]`, `exec [--hermetic --inputs=... --no-network] [--explain] <command>`.
**Cache**: `cache push`, `cache info`, `cache clean`.
**Project**: `init [--detect]`, `config`.
**Meta**: `status`, `version`.

### 18.2 Common flags

Configuration: `--target`, `--opt-level`, `--lto`, `--sanitizer`, `--debug-info`, `--coverage`.
Cache: `--no-cache`. (Remote flags `--remote`, `--require-remote`, and `--no-workers` arrive with v1.x RE and workers.)
Output/diagnostics: `--verbose`, `--explain`, `--output=json|human`, `--continue-on-error`, `--warnings-as-errors`, `--suppress-category=<cat>`.
Test: `--filter=<pattern>`, `--affected --since=<commit>`. (`--shard-count`, `--retries`, `--inspect` arrive with v1.x test features.)

### 18.3 Not in v1

`watch`, `repro`/`debug`, `cargo <subcommand>` wrappers, command aliases. All additive later.

---

## 19. Reference Schemas

### 19.1 Action specification

```
action:
  name:                  String        # human-facing identifier within the rule
  command:               [String]
  inputs:                {name: ref}   # files, dirs, other actions' outputs, toolchains, FileSets
  outputs:               {name: path}  # expected output paths
  env:                   {name: value} # keys AND values in cache key
  working_directory:     String        # default "."
  platform_requirements: Platform?
  execution_mode:        sealed | permeable | native            # default sealed
  cache_policy:          deterministic | non_cacheable | snapshot_based   # default deterministic
  snapshot_paths:        [String]      # for snapshot_based caching
  worker:                String?       # v1.x
  timeout_ms:            Integer       # default 600000
  diagnostics_paths:     [String]
```

Cache key: command, input content hashes, env (keys+values), working_directory, execution_mode, cache_policy + snapshot_paths, worker, relevant platform requirements, consumed axes, sandbox version. Excludes timestamps, action name, host environment.

### 19.2 Test result

```
TestResult:
  test_target:   Label
  configuration: Configuration
  outcome:       passed | failed | skipped | errored | timed_out | passed_after_retry
  duration_ms:   Number
  cases: [ { name, outcome, duration_ms, failure_message?, failure_stack?, stdout?, stderr? } ]
  shard_info:    { shard_index, total_shards }?    # v1.x
  retry_count:   Number                            # v1.x retries; 0 in Milestone 1
```

Case identity (`test_target` + `name`) is stable across builds for external history tooling.

### 19.3 Diagnostic

```
Diagnostic:
  severity:           error | warning | info | hint
  code:               String        # "TS2304", "MB-RUST-001"
  message:            String
  source:             { file, line, column?, end_line?, end_column? }
  category:           String        # "type-check", "lint", "compile", "test", ...
  long_message:       String?
  suggestions:        [ { description, edits: [{ file, range, replacement }] } ]
  related_locations:  [ { file, line, column?, message } ]
  rule_attribution:   Label
  action_attribution: ActionId?
  tags:               [String]
```

### 19.4 Label grammar

```
label        = repo? "//" package_path (":" target_name)?
repo         = "@" identifier                    # reserved; v1 default workspace only
package_path = identifier ("/" identifier)*
target_name  = [a-zA-Z0-9_][a-zA-Z0-9_\-.]*
identifier   = [a-zA-Z0-9_][a-zA-Z0-9_\-]*
```

`//crates/my_lib:my_lib` (full); `//crates/my_lib` (implies `:my_lib`); `:target` (package-relative); `//crates/...` (recursive glob); `//crates/my_lib:*` (target glob). Canonical form starts `//` with explicit target name; case-sensitive on all platforms.

### 19.5 WORKSPACE (sketch)

```python
workspace(name = "my_project")
register_toolchain(name = "rust_1_78", version = "1.78.0")
register_toolchain(name = "node_20", version = "20.10.0")
set_default_platform("linux-x86_64")
```

Plus an optional, gitignored, user-level `.mybuild/config.toml` for machine-specific settings. Exact toolchain-registration API finalized during implementation.

---

## 20. Benchmark Gates

The thesis is considered **unvalidated** until Anneal beats or matches native tooling plus common CI caches for representative Cargo and pnpm repositories. This is a first-class gate, not a soft aspiration: Milestone 1 is not "done" until these pass.

### 20.1 Comparison baselines

For representative repos, benchmark Anneal against:

- Raw Cargo + GitHub Actions cache
- Cargo + sccache
- pnpm + GitHub Actions cache
- Turborepo / Nx (for the JS/TS side)
- Bazel / Buck2 / Pants (where a comparable setup exists)

### 20.2 Scenarios

Each baseline measured across: cold-cache full build; warm-cache no-op; warm-cache single-package change; warm-cache shared-dependency change; affected-test selection latency; and CI cold-start (warmed from a prior run's cache).

### 20.3 Pass criteria

- **Incremental builds** (the common case): Anneal must clearly *beat* the native baselines (this is where the content-addressed cache and snapshot protocol should win decisively).
- **Cold-cache and workspace-wide changes**: Anneal must *match* within an acceptable margin (the per-`(crate, test_type)` overhead and per-invocation Cargo startup are the known costs; if overhead trends toward the high end of the ~5–20% estimate, prioritize the batch-invocation optimization).
- **CI cold-start**: Anneal must clearly beat native baselines via precise content-addressed cache restore and cross-PR sharing.

If incremental builds do not beat native tooling, the thesis is not validated and the design must be reconsidered before proceeding to v1.x.

---

## 21. Deferred Features and Known Limitations

### 21.1 Deferred to v1.x (all additive / non-breaking)

- `nextjs_app`; **Rust → WASM → TS** typed-binding path (the cross-language type-safety demonstration)
- **Remote cache backend**, then **remote execution**
- **Persistent TypeScript worker**
- **Resourceful tests** (service provisioning), sharding, flakiness retries, richer test infra
- **Daemon / RPC** interface
- Python (`uv_workspace`) and Go (`go_module`) rules
- Build-mode files and per-target default modes (CLI flags only in v1)
- **Named output groups and dependent variant selection** ([§5.5](#55-providers-outputs-and-metadata)–[§5.6](#56-variant-menus-and-selection)): the provider model is defined now, but Milestone 1 ships only the default output group; the named-group menu and edge-level selection are additive.
- **Demand-driven output pruning**: build only the provider outputs a build actually consumes — the mechanism that makes multi-variant menus non-wasteful ([§5.6](#56-variant-menus-and-selection)). Until then, demand follows the dependency graph (one target per variant).
- **Tree / directory artifacts**: outputs whose member set is known only at execution time (e.g. a generated package directory), the companion to the static named-group menu ([§5.6](#56-variant-menus-and-selection)). The directory-walk machinery already exists in the snapshot protocol ([§8.2](#82-the-snapshot-protocol-for-stateful-caches)).

### 21.2 Deferred further (post-v1 / v2)

- Structured event protocol (Milestone 1 emits internal logs, deliberately not a stable consumer API; test-result case identity is settled now to keep future tooling possible)
- **Secrets / credentials and private-registry support** — see [§7.5](#75-secrets-and-private-registries--deliberately-out-of-scope)
- Public rule API / third-party extensibility (first-party only; experimental internal API may be exposed but not as a contract)
- Third-party rule registry (filesystem loading only)
- Bazel/Buck2 migration tooling
- User-defined configuration axes and custom transitions
- Aspect-equivalent graph-traversal primitive (diagnostics channel covers the common case)
- Deep semantic snapshot pruning (e.g., Cargo `target/` fingerprint-level pruning — experimental, not promised)
- Diagnostics aggregation/suppression UX; cross-build history tooling
- `watch`, `repro`, `cargo <subcommand>` wrappers; batch test-invocation optimization
- C/C++/JVM rules; framework rules beyond Next.js; npm/yarn; Poetry/pip-tools
- Windows support

### 21.3 Known limitations

- **Milestone 1 proves routing, not cross-language type safety** ([§2.3](#23-what-milestone-1-does-not-prove)).
- **No private-registry support**; Milestone 1 is scoped to public-dependency workflows.
- macOS hermeticity is best-effort, not strict (Linux-VM mode available).
- Direct `cargo build` is not guaranteed when generated inputs or sandbox-controlled env are involved (use the materialized or mediated modes).
- Per-`(crate, test_type)` granularity has a modest cold-cache/workspace-wide-change overhead vs. `cargo test --workspace`.

---

## 22. Open Questions and Risks

**starlark-rust integration.** Assumed clean support for custom error messages, source-location preservation, registering rule primitives as globals, and monorepo-scale performance — not yet verified end-to-end. *Action: validate early; this underpins the entire BUILD layer.*

**The snapshot protocol's correctness-neutrality in practice.** The central invariant ([§1.4](#14-core-hypothesis-and-the-central-invariant)) is easy to state and hard to guarantee — a restored `target/` that subtly changes output would be a severe, hard-to-detect bug. *Action: build verification (cold build vs. snapshot-restored build, diff outputs) into the test strategy from day one; treat any divergence as a release blocker.*

**Per-`(crate, test_type)` cold-cache overhead.** Estimated 5–20% vs. `cargo test --workspace`. *Action: this is a benchmark gate ([§20](#20-benchmark-gates)); if overhead trends high, prioritize the batch-invocation optimization.*

**macOS materializer at scale.** Hardlink-from-CAS on APFS is not battle-tested here (file locking, per-inode hardlink limits, cross-filesystem fallback). *Action: benchmark on macOS early.*

**macOS hermeticity ceiling.** `sandbox-exec` is deprecated (still functional), not strict, with no public successor. Linux-VM mode covers strict needs at a performance cost. *Action: document honestly; monitor Apple's tooling.*

**The system/rule boundary under future extensibility.** When the rule API opens, "policy in system, mechanism in rules" must hold to avoid Bazel's fuzzy-boundary problems. *Action: maintain the boundary now, while rules are first-party.*

**Experimental internal rule API becoming a de facto contract.** Exposing the internal API experimentally risks adopters depending on it. *Action: keep it undocumented-as-contract and visibly unstable; resist feature requests that imply stability commitments.*

**Nix flake maintenance.** Conventions evolve; the flake needs upkeep. *Action: clarify community- vs. core-maintained.*

---

*End of v1 design document.*
