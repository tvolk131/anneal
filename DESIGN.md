# Anneal architecture

> **Status:** Current architecture overview, last reconciled July 14, 2026.
> The [README](README.md) owns feature availability. The contract documents under
> [`docs/`](docs/README.md) own user- and rule-author-facing guarantees. Proposals describe
> possible future work and are not part of this architecture.

Anneal is a pre-1.0 build system for polyglot repositories. It coordinates native tools
through an explicit target and action graph instead of replacing their package managers,
compilers, or lockfiles.

The current system has four defining properties:

1. Native tools remain responsible for ecosystem semantics.
2. Anneal makes cross-tool dependencies and generated artifacts explicit.
3. Actions execute behind declared, platform-graded sandbox boundaries.
4. Reuse is an optimization over the same action contract, never a different correctness
   path.

## 1. Product boundary

Anneal wraps Cargo, pnpm, Nickel, and generic commands. A rule translates a coarse native
package or operation into targets, providers, artifacts, actions, and persistent-state
declarations. The native tool still interprets its manifest and performs its build.

This boundary deliberately favors low-conversion adoption over Bazel-style decomposition.
Anneal may initially model an entire Cargo or pnpm workspace as a coarse unit. Coarseness can
cause extra work, but the declared sandbox boundary is intended to prevent it from causing
stale reuse.

The current product does not provide remote caching, remote execution, automatic toolchain
provisioning, staged graph analysis, or general input sensing. See the
[roadmap](docs/roadmap.md) for their status.

## 2. Loading, analysis, and execution

Anneal evaluates a restricted Starlark BUILD surface in three conceptual stages:

1. **Loading** discovers packages and rule declarations.
2. **Analysis** resolves configured targets, providers, artifacts, actions, and state.
3. **Execution** schedules the resulting action graph, materializes inputs, runs commands,
   captures outputs, and records reusable results.

The phases are structurally separate today. Analysis-time `QuerySpec` infrastructure can run
sealed, cached native-tool queries, but no production rule uses it yet. Generated content can
be consumed by execution actions; content that must reshape the target graph requires an
explicit materialization/re-analysis workflow or future staged analysis.

Ordinary `build` and `test` currently execute the analyzed action set. Demand-driven output
pruning is not implemented. `materialize` is narrower: it selects the producer subgraph for
the routed files needed by the requested consumer.

## 3. Targets, actions, and artifacts

Targets are user-facing graph nodes identified by labels. Rules analyze targets into typed
providers and actions. Providers carry artifacts and metadata across dependency edges.

Artifacts have symbolic identity before execution and content identity after capture. A
generated artifact can therefore move from one native-tool boundary into another without
being checked into source control or discovered through an ambient worktree path.

The current routing model supports file-shaped outputs whose paths are known during analysis.
Dynamic tree artifacts and output sets discovered only after a tool runs remain future work.

## 4. Generated-file routing and materialization

Actions consume generated artifacts through declared sandbox paths. This is the normal
in-graph path: the producer runs, Anneal captures its output, and downstream actions receive
the content through an explicit edge.

`anneal materialize` is the bridge for IDEs and unwrapped tools that require generated files
in the source tree. It mirrors the generated inputs routed to a consuming target, tracks
ownership and digests, avoids needless mtime changes, and refuses destructive replacement by
default. Materialization is explicit and does not automatically trigger a second analysis
pass.

## 5. Execution and sandboxing

Actions select one of three execution modes:

- `Sealed` runs behind the strongest boundary the host platform provides.
- `Permeable` uses a scrubbed environment without OS-level isolation and is non-cacheable.
- `Native` runs directly on the host and is non-cacheable.

On Linux, sealed execution uses Bubblewrap namespaces and receives the enforcement grade
`Enforced`. On macOS, sealed execution uses a deny-by-default Seatbelt policy and is graded
`LoudBestEffort`; it is not represented as Linux-equivalent isolation.

The normative boundary is documented in
[`docs/sandbox-contract.md`](docs/sandbox-contract.md). Implementation mechanics, including
warm directories and snapshot handling, live in
[`docs/sandboxing.md`](docs/sandboxing.md).

## 6. Local reuse, trust, and provenance

The executor has a local content-addressed store, action-result cache, and snapshot store.
Cache entries record provenance and a computed trust tier. `--require-enforced` can impose a
minimum enforcement floor. `Promotable` currently records eligibility only; no remote backend
uploads or downloads entries.

An action-cache hit must be observationally interchangeable with executing the same action.
Persistent native-tool state follows the same correctness-neutral invariant: warm and cold
execution may differ in cost, not in declared outputs or success.

Several pre-1.0 correctness-hardening tasks remain before Anneal should make a strong
shared-cache promise. They are tracked at the top of [`TODO.md`](TODO.md), including complete
action identity, file-digest memoization, generic action cache policy, and persistent-state
owner identity.

## 7. Persistent native-tool state

Rules can declare typed persistent state for tools such as Cargo's `target/` and pnpm's
`node_modules`. State is either phase-separated or interleaved; analysis rejects reading
interleaved state as an ordinary dependency.

Snapshot owners use reusable warm directories by default. Anneal synchronizes changed
declared inputs into the directory, preserves tool state in place, and uses a commit record to
recover safely after interruption. Private working snapshots are local accelerators and are
not cross-machine artifacts.

Portable reuse belongs in the content-addressed layer: declared action outputs and future
portable ecosystem stores. Anneal does not treat Cargo `target/` or `node_modules` snapshots
as remotely transportable build products.

## 8. Configuration and the focus cone

A configured target combines a target with the current configuration axes. Actions include
only the axes they consume in their identity.

For local `build` and `test`, Anneal derives a dirty focus cone from `git status`. Dirty
targets and their reverse dependents run in `Incremental` mode; the remainder run in
`Hermetic` mode. The analyzer enforces the monotonicity rule that an incremental target may
not depend on a hermetic target.

This is experimental. There is no hysteresis or pinning, and committing a tree can move a
target between action contracts and cause a rebuild.

## 9. Current first-party rule boundary

- `cargo_workspace` wraps a Cargo workspace as a coarse unit, uses pinned Nix toolchains,
  manages warm `target/` state, and splits test compilation from execution.
- `pnpm_workspace` performs frozen offline installation into managed state and runs declared
  scripts.
- `nickel_eval` evaluates a self-contained Nickel source and exposes a selected serialized
  output.
- `filegroup`, `alias`, and `genrule` provide generic graph and routing primitives.

The [README status matrix](README.md#current-implementation-status) is the authoritative list
of limitations for each rule.

## 10. Design records and future work

Accepted decisions live in [`docs/decisions/`](docs/decisions/). Future mechanisms live in
[`docs/proposals/`](docs/proposals/README.md). Historical design conversations, milestone
plans, measurements, and reconciliation notes live in [`docs/archive/`](docs/archive/README.md)
and are not current product or implementation references.
