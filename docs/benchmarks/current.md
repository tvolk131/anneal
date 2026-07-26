# Current benchmark findings

> **Status:** Engineering measurements, last consolidated July 14, 2026.
> Results are indicative rather than release guarantees and should be refreshed when the
> executor, snapshot protocol, or benchmark environment changes materially.

## What the benchmark asks

The Cargo harness compares Anneal with the same Cargo invocation wrapped by the current
`cargo_workspace` rule. Both paths set `CARGO_INCREMENTAL=0`, so the comparison isolates
Anneal's orchestration, sandbox, cache, and managed-state costs. It does not compare Anneal
against Cargo's normal developer-default rustc incremental-codegen configuration.

Run the harness with:

```console
cargo run -p anneal-bench --release [-- N]
```

The important scenarios are:

- cold execution;
- warm execution after a small source change;
- exact no-op action-cache reuse;
- a fresh checkout with a populated local Anneal store;
- realistic large `target/` directories;
- generated outputs whose bytes do not change.

## Current observations

Measurements recorded during the warm-directory and private-snapshot work showed:

- exact no-op cache hits roughly 10–55 times faster than invoking Cargo in the synthetic
  workspace;
- fresh checkouts with a populated local Anneal store roughly 90–300 times faster than native
  Cargo without `target/` in that synthetic scenario;
- warm one-package changes in trivial multi-package workspaces approximately 36–58% slower
  than the equivalent native invocation;
- removing synchronous private-snapshot saves reduced a heavier `syn` warm-edit comparison
  from roughly +62% to roughly +27% over native;
- a separate `serde` experiment still showed roughly +109% on a warm one-change miss, while
  file-digest memoization improved its no-op path to roughly 1.7 times native.

These numbers vary by machine and workspace shape. Trivial crates exaggerate fixed wrapping
costs, while large working directories expose snapshot and filesystem costs that trivial
compile-time percentages can hide.

## Interpretation

The data supports a scenario-based performance promise:

> Anneal should win decisively when it can avoid invoking the native tool, and should impose
> bounded overhead when it must invoke that same tool.

It does not support “every incremental Anneal build must beat native Cargo.” Native Cargo is
already the lower bound for a wrapped cache miss except where Anneal can avoid work outside or
inside that invocation.

Cross-machine reuse of a changed Cargo workspace is also not solved by shipping `target/`.
Snapshots are local working state. Portable changed-workspace acceleration likely requires a
compiler/object cache or another ecosystem-aware content-addressed layer.

## Required reporting

Future benchmark updates should record:

- hardware, operating system, filesystem, and build profile;
- the exact native command and environment;
- whether rustc incremental codegen is enabled;
- cold, no-op, and changed-input distributions rather than one sample;
- analysis, materialization, restore, native run, capture, save, and teardown timings;
- working-state size and file count;
- whether the result came from exact action reuse, native incremental reuse, or full execution.
