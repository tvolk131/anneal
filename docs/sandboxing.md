# Sandboxing, snapshots, and warm state

> **Status:** Current implementation mechanics, last reconciled July 14, 2026.
> The normative rule-author guarantee is the
> [`Sealed Sandbox Contract`](sandbox-contract.md). This document explains how the executor
> realizes that boundary and where current limitations remain.

## 1. Materialization and isolation are separate

Every action starts from two layers:

1. **Materialization** places declared source blobs and producer outputs at their action-relative
   paths and captures declared outputs afterward.
2. **Isolation** determines what other filesystem, network, process, and environment surfaces
   the command can observe.

The materialization model is shared across platforms. The isolation backend is platform-specific.

## 2. Store-safe input materialization

| Platform | Ordinary declared input | Writable declared input | CAS safety |
|---|---|---|---|
| Linux | Hardlink, overmounted read-only inside sealed Bubblewrap execution | Private copy | Sealed command cannot mutate the shared inode |
| macOS/APFS | `clonefile` copy-on-write clone plus read-only permissions | Private copy | Clone has a distinct inode; writes cannot modify the CAS blob |
| Fallback/cross-volume | Copy plus read-only permissions | Private copy | Distinct file |

Writable inputs exist for native tools that perform harmless atomic replacement or bookkeeping
on files whose original digest still defines the action. They are private sandbox copies, never
permission to modify repository or CAS content.

Declared outputs are captured only from their declared paths. Missing or unexpected outputs are
handled according to the action's output contract; output path identity still has a known digest
gap tracked in [`TODO.md`](../TODO.md).

## 3. Linux sealed execution

Linux sealed actions run through Bubblewrap. The prepared `/work` tree, private home/temp paths,
synthetic account files, declared toolchain roots, a private proc/device view, and explicitly
allowed runtime surfaces form the visible filesystem.

The wrapper:

- uses user, mount, PID, IPC, UTS, and network namespaces;
- denies network by default;
- enables network only for fixed-output actions carrying that capability;
- supplies normalized UID/GID, hostname, environment, home, temp, and working paths;
- disconnects parent standard input and closes inherited file descriptors above stderr;
- fails before command execution if the required sandbox backend cannot be used.

Undeclared host paths are structurally absent. Linux sealed execution is graded `Enforced`.

Kernel version, CPU topology, time, randomness, and selected `/proc`/device surfaces remain
observable. Sealing is therefore an input boundary, not a proof of output byte determinism.

## 4. macOS sealed execution

macOS sealed actions run through a generated deny-by-default Seatbelt profile. The profile
allows the prepared action tree, private home/temp paths, declared toolchain roots, and a narrow
Darwin runtime allowlist. Network is denied unless the action carries the fixed-output network
capability.

Seatbelt denies undeclared reads and writes, but it is policy interception rather than Linux
mount-namespace absence. Metadata visibility, the runtime allowlist, and the deprecated
`sandbox-exec` interface prevent Anneal from claiming Linux-equivalent enforcement. macOS sealed
execution is graded `LoudBestEffort`.

Hard filesystem hermeticity currently requires running on Linux. A Linux VM is only a
[proposal](proposals/linux-vm.md).

## 5. Undeclared-input diagnostics

The current sandbox enforces the boundary but does not have a general read-event tracer.

- Linux tools usually observe an ordinary missing-path failure for an undeclared input.
- macOS observes a Seatbelt denial.
- Anneal does not yet reliably translate either event into a stable diagnostic naming the
  BUILD attribute to edit.

The [input-sensing proposal](proposals/input-sensing.md) may improve authoring and diagnostics.
Observation will not silently remove declared inputs or widen an execution boundary.

## 6. Snapshots

A snapshot is a locally stored native-tool working tree such as Cargo `target/` or pnpm
`node_modules`. It is managed state, not a declared build output.

Two policy shapes use snapshots:

- A `SnapshotBased` action owns the state, restores it before execution when needed, and may
  save it afterward.
- A `SnapshotConsuming` action restores another action's state read-only and always executes.

Snapshot correctness is neutral: deleting the snapshot may make the next build slower but may
not change its declared result. Anneal samples that invariant with cold-versus-warm verification.

Snapshots are local-only and are never intended for cross-machine transport.

## 7. Warm owner directories

Snapshot owners use stable warm directories by default. Instead of reconstructing and deleting
the native working tree for each invocation, Anneal keeps it in place and synchronizes only
changed declared inputs.

The warm path is:

1. Lock the state key across threads and processes.
2. Check the commit record. An absent record means a prior mutation was interrupted, so the
   directory is discarded or restored from the last local snapshot.
3. Diff the recorded declared-input manifest against the new action inputs.
4. Delete removed inputs and replace changed inputs with distinct-inode copies carrying fresh
   mtimes. Leave unchanged files untouched so native fingerprinting can reuse them.
5. Clear the commit record before running the command.
6. Capture declared outputs and atomically write the new manifest/commit record only after
   successful completion.

Owners sharing one state key serialize on the same warm directory. Different keys remain
parallel.

Consumers do not access the mutable warm directory. They restore a committed local snapshot
into a fresh sandbox.

## 8. State-key and lifecycle limitations

The current state key is rule-scoped and includes rule-provided shard values such as toolchain,
lockfile, platform, configuration axes, state kind, and attestation epoch. It does not
independently include complete workspace/package/target identity. Byte-identical workspace
copies can therefore collide if the shard does not otherwise distinguish them. This is
Priority 0 hardening work.

The action model currently carries one state tree per action. Phase-separated single-producer
uniqueness is not structurally complete, and every snapshot owner is conservatively capped at
the local trust tier.

No store garbage collection or `anneal clean` command exists. Warm directories, snapshots,
action results, and CAS blobs can grow without bound.

## 9. Private and shared local snapshots

“Private” and “shared” describe local sibling consumption, not cross-machine portability.

- Cargo `target/` is private native-tool state. The warm directory is the live copy, so the
  owner can skip a synchronous per-build snapshot save when no local consumer needs it.
- pnpm `node_modules` is shared with local script actions. The install owner must commit a local
  snapshot that consumers can restore even when the install action later exact-cache-hits.

Neither state tree is a remote cache artifact.

## 10. Portability boundary

Cross-machine reuse belongs in the content-addressed layer:

| Ecosystem | Local working state | Portable direction |
|---|---|---|
| Cargo | `target/` | Vendored/registry inputs, ordinary outputs, and a possible future compiler cache |
| pnpm | `node_modules` | Lockfile-pinned package store plus ordinary script outputs |

Shipping `target/` or `node_modules` would entangle path layout, platform behavior, mutable tool
state, and reproducibility. Anneal instead intends to ship immutable content and re-materialize
native working state locally.

## 11. Verification and remaining work

The current suite covers store-corruption resistance, platform sandbox boundaries, warm-state
reverts, cold/warm neutrality for representative Cargo behavior, commit-record recovery, and
same-key serialization.

Remaining work includes:

- broader neutrality coverage across rule/action shapes;
- stronger state-owner identity;
- store retention and garbage collection;
- subprocess-level CLI coverage;
- portable pnpm store separation; and
- a production rule using the analysis-query substrate.

Current performance methodology is recorded in
[`benchmarks/current.md`](benchmarks/current.md). The archived
[long-form sandbox design](archive/sandboxing-design-and-measurements.md) preserves the earlier
mechanical derivation and benchmark investigation.
