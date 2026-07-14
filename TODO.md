# Anneal open work

> **Status:** Active engineering backlog. This file contains open work only.
> The [README](README.md) owns current availability, the
> [roadmap](docs/roadmap.md) owns product sequencing, and the archived
> [engineering log](docs/archive/engineering-log.md) preserves completed investigations and
> historical measurements.

## Priority 0 — correctness and trust

- [ ] Include the complete declared output map in action identity and add collision tests for
      renamed logical outputs and changed destination paths.
- [ ] Harden the file-digest memoization contract so a same-size, same-mtime content change
      cannot reuse a stale digest. Decide the role of inode, ctime, file watchers, and a
      paranoid verification path before remote publication exists.
- [ ] Reconcile generic action cache policy with the rule contract. Arbitrary `genrule`
      commands must not silently receive deterministic/cacheable treatment without an
      explicit, enforceable policy.
- [ ] Include sufficient workspace/package/target identity in persistent-state keys so
      distinct owners with otherwise identical toolchain and configuration inputs cannot
      collide.
- [ ] Expand cold-versus-warm correctness-neutrality verification across representative
      Cargo and pnpm pipelines, including content reverts and interrupted warm-state commits.
- [ ] Audit every action-identity field against the rule contract and add a test that fails
      when a newly added identity field is omitted from hashing.

## Priority 1 — production operation

- [ ] Implement garbage collection and retention for CAS blobs, action results, snapshots,
      warm directories, and fixed-output downloads.
- [ ] Add cache/store inspection and cleanup commands with dry-run support.
- [ ] Implement demand-driven action pruning so `build` and `test` execute only actions whose
      outputs are required by the requested operation.
- [ ] Define interruption, cancellation, and concurrent-process behavior for all persistent
      stores and cover it with process-level tests.
- [ ] Add subprocess-level CLI tests for materialization, refusal behavior, cleanup, and
      consumer-subgraph selection.
- [ ] Decide whether aliases should forward or explicitly reject materialization routes.
- [ ] Add an explicit staged re-analysis workflow for generated manifests needed by native
      tools outside Anneal.

## Priority 2 — diagnostics and adoption

- [ ] Add stable diagnostic codes and actionable undeclared-input messages based on behavior
      the current sandbox can actually report.
- [ ] Close the untracked-file gap in `affected --since` without sweeping ignored build junk.
- [ ] Add `affected --explain` and `why --all` after defining bounded output behavior.
- [ ] Add user-facing toolchain registration or provisioning so first-party rules do not
      require `ANNEAL_TOOLCHAIN_MANIFEST` and Nix.
- [ ] Add an onboarding workflow only after it can emit configuration that works without
      overwriting existing repository files.
- [ ] Add store-size and cache-hit diagnostics before encouraging long-lived production use.

## Performance and CI

- [ ] Replace “incremental must beat native” with measured gates matching the architecture:
      decisive wins when Anneal avoids native work, bounded overhead when it invokes the same
      tool, and correctness-neutral warm reuse.
- [ ] Add a representative benchmark matrix covering no-op, one-file edits, dependency edits,
      generated-output early cutoffs, clean checkout with a warm local CAS, and large warm
      tool-state directories.
- [ ] Investigate the remaining heavy-workspace warm overhead without relying on trivial
      workspaces as the sole percentage gate.
- [ ] Design portable ecosystem stores for cross-machine Cargo and pnpm reuse; do not transport
      `target/` or `node_modules` working snapshots.
- [ ] Define remote-cache admission, trust-floor, namespace, protocol, and poisoning tests
      before implementing a backend.

Current benchmark methodology and indicative measurements live in
[`docs/benchmarks/current.md`](docs/benchmarks/current.md).

## Rule completeness

### `cargo_workspace`

- [ ] Stage authoritative `cargo metadata` through the query/state model.
- [ ] Support binaries, binary unit tests, and per-integration-binary targets.
- [ ] Support separately addressable tests and richer structured test results.
- [ ] Support generated lockfiles through an explicit staged workflow.
- [ ] Support pinned non-crates.io dependency acquisition.
- [ ] Define the relationship between warm Cargo fingerprint reuse,
      `CARGO_INCREMENTAL=0`, and a future portable compiler cache.

### `pnpm_workspace`

- [ ] Populate a portable pnpm store from lockfile-pinned fixed-output acquisitions.
- [ ] Separate portable store content from local `node_modules` working state.
- [ ] Model lifecycle and native-build actions explicitly.
- [ ] Add structured JavaScript test results and test-result policy.
- [ ] Add generated-package name-resolution routing after its dependency and tree-artifact
      requirements are explicit.
- [ ] Add a meaningful snapshot-restore neutrality test using nontrivial dependencies.

### `nickel_eval`

- [ ] Declare and support multi-file Nickel import closures.

### Generic rules

- [ ] Define a safe cacheability path for arbitrary commands.
- [ ] Add tree artifacts and dynamic output sets only with explicit identity and capture
      semantics.

## Analysis and scale

- [ ] Put a production rule through `QuerySpec`, beginning with a registry-free Cargo metadata
      query.
- [ ] Allow queries to consume phase-separated state without admitting arbitrary generated
      action artifacts into analysis.
- [ ] Add incremental analysis persistence only after measuring whole-process analysis as a
      real bottleneck.
- [ ] Add package-owner validation for sweeping workspace rules.
- [ ] Validate Starlark loading and analysis performance at monorepo scale.

## Proposals, not current commitments

The following have design value but are not required to establish Anneal's current thesis:

- [Input sensing](docs/proposals/input-sensing.md)
- [Remote/shared cache](docs/proposals/remote-cache.md)
- [Linux VM execution](docs/proposals/linux-vm.md)
- [Simplex rules](docs/simplex-rules.md)
- Staged graph analysis
- Remote execution
- Persistent workers
- General third-party rule API stability

Promote an item out of this section only when its prerequisite, product use case, and
correctness gate are explicit.
