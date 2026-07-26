# `pnpm_workspace`

> **Status:** Current first-party rule reference, last reconciled July 14, 2026.
> The rule is `Partial`; the [README status matrix](../README.md#current-implementation-status)
> is authoritative.

`pnpm_workspace` wraps a pnpm workspace in two layers:

1. a frozen offline installation that owns managed state; and
2. explicitly declared build or test scripts that consume that state.

pnpm remains authoritative for lockfile, workspace, installation, and script semantics. Anneal
declares the sandbox and graph boundaries around those operations.

## Current capability

| Area | Current behavior |
|---|---|
| Workspace discovery | Reads `pnpm-workspace.yaml` and package manifests |
| Installation | Runs frozen offline installation with lifecycle scripts disabled |
| State | Phase-separated pnpm installation state, committed for local script consumers |
| Scripts | Runs only scripts explicitly named in the BUILD rule |
| Script kinds | Build or test, declared explicitly rather than inferred from the name |
| Generated inputs | Routes declared data to explicit relative paths |
| Toolchain | Node, pnpm, and POSIX runtime resolved from `ANNEAL_TOOLCHAIN_MANIFEST` |
| Environment | Scrubbed action environment plus declared values |
| Network | Denied during install and scripts in the current supported path |
| Cache tier | Snapshot owners conservatively local; scripts do not receive remote promotion |

External package acquisition, lifecycle/native-build actions, structured JavaScript test
results, generated-package name resolution, and a portable pnpm store are not implemented.

## Installation action

The install action consumes the committed lockfile, workspace/package manifests, platform,
toolchain identity, and rule configuration. It runs pnpm in frozen offline mode and owns the
phase-separated installation state.

The lockfile is provided as a private writable input because pnpm may perform atomic temporary
file/rename behavior even when it is not allowed to change dependency resolution. The original
declared digest still defines the action input, and the private copy prevents repository or CAS
mutation.

Lifecycle scripts are not silently admitted into the install boundary. They may execute
arbitrary code, perform network access, or compile native extensions, so supporting them requires
explicit action modeling.

The current rule assumes required package content is already available offline. It does not
populate a registry-backed store from `pnpm-lock.yaml` integrity records.

## Script actions

Each configured script is a dependent action that restores the committed installation state
read-only into its own sandbox. Script selection is explicit because names such as `build`,
`test`, `check`, and `dev` have no universal pnpm semantics.

The declared script kind determines intent:

- a build script may declare outputs exposed through the target's file provider;
- a test script is reported as test execution and consumes test-relevant configuration.

Anneal does not automatically run `dev`, `start`, `prepare`, `postinstall`, or any other script
discovered in `package.json`.

Generic script cacheability is conservative product territory. Arbitrary scripts may read time,
randomness, process information, or other visible surfaces even when sealed. The known generic
action cache-policy deviation is documented in [`rules.md`](rules.md#known-generic-action-deviation)
and must be resolved before script promotion is advertised.

Structured per-case JavaScript test parsing, retry/flake policy, and separately addressable
script/test targets are future work.

## Generated-file routing

The current rule supports plain-path data routing. A generated artifact from another target is
placed at an explicit relative destination for the declared script action.

This is a clean execution edge:

- installation does not depend on the generated file;
- editing the generated source rebuilds its producer and consuming script;
- the generated artifact's digest enters the script action identity; and
- `anneal materialize` can mirror the routed file for an IDE or unwrapped pnpm command.

Name-based generated-package routing through `file:` or `workspace:` dependencies is not
implemented. It requires clearer generated package metadata, possible multi-file/tree routing,
and—in cases where a generated manifest changes workspace structure—an explicit staged-analysis
boundary.

## Persistent-state model

pnpm installation state is phase-separated:

- the install action produces it;
- script actions consume a committed local snapshot read-only; and
- consumers never mutate the owner's warm directory directly.

The current state includes working-tree-shaped content such as `node_modules`. That state is
local-only and is not suitable for cross-machine transport.

The desired portable layer is a lockfile-pinned content-addressed package store. A future
implementation would acquire immutable package content, then run offline installation locally to
re-link `node_modules` for the current machine. This is roadmap work, not current behavior.

## Configuration

The current rule consumes only axes relevant to the action it emits. Platform affects
installation and any platform-sensitive scripts. Build-oriented axes apply only where the rule
maps them to a declared build script; test-oriented axes apply to test actions.

Unrelated configuration should not invalidate an action. Adding an axis mapping requires an
identity test demonstrating both sides: changing a consumed axis changes the action key, while
changing an ignored axis does not.

## Known limitations

- No external registry acquisition or portable pnpm content store.
- No lifecycle or native-addon build actions.
- No structured JavaScript test-result protocol.
- No reproducibility graduation for arbitrary scripts.
- No separately addressable script/test targets or demand-driven output pruning.
- No generated-package name resolution.
- No toolchain provisioning outside the Nix manifest path.
- No meaningful snapshot-restore neutrality fixture with nontrivial external dependencies yet.

The archived [long-form pnpm design](archive/pnpm-workspace-design.md) preserves the original
Milestone 1 tradeoff analysis and deferred alternatives; it is not the current rule reference.
