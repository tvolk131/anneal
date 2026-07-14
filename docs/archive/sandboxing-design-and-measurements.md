# Sandboxing, materialization, and input hermeticity — archived design

> **Archived:** This file preserves the long-form sandbox/warm-state derivation and historical
> measurements. It is not the current mechanics or contract. See
> [`../sandboxing.md`](../sandboxing.md), [`../sandbox-contract.md`](../sandbox-contract.md),
> and [`../benchmarks/current.md`](../benchmarks/current.md).

## 1. Two layers: materialization vs. isolation

An action runs in a per-action sandbox built from two independent layers:

- **Materialization** (CAS ↔ filesystem) — *where the bytes go*. Declared inputs are
  placed into the sandbox at their expected paths; declared outputs are captured back
  into the CAS afterward. Shared across platforms.
- **Isolation** (the sandbox proper) — *what the action may do*. Network, filesystem
  visibility, environment. Platform-specific.

The materializer is identical everywhere; only the isolation layer differs.

## 2. Materialization — and why a materialized input can't corrupt the store

Inputs are placed with the cheapest store-safe mechanism for the platform:

| Platform | Mechanism | Store-corruption safety |
|----------|-----------|-------------------------|
| **Linux** | ordinary inputs: hardlink from the CAS (shared inode); writable inputs: private copy | sealed actions overmount ordinary declared inputs read-only inside `bubblewrap`; writable inputs are distinct files, so tool mutations cannot affect the CAS |
| **macOS / APFS** | ordinary inputs: `clonefile` (copy-on-write, **distinct inode**) + `chmod 0444`; writable inputs: private copy | a write COWs for ordinary inputs, and writable inputs are already private copies — the store blob is never mutated; per-inode hardlink limit sidestepped |
| any, cross-volume | copy + `0444` | (fallback) |

The macOS choice is deliberate: a hardlink shares the inode, so a misbehaving action
that wrote to a materialized input would mutate the immutable store. A CoW clone makes
that impossible (proven by a test that clears the read-only bit, overwrites the input,
and confirms the store blob is intact).

Writable inputs are an explicit action contract for tools that rewrite an input manifest
as private scratch while still being deterministic with respect to the original digest.
They remain part of the action key. Warm snapshot owners refresh writable inputs before
every reuse because a previous run may have edited the on-disk copy.

Output handling: parent directories for declared output paths are pre-created, so an
action can write a nested output (`gen/config.json`) without `mkdir`-ing itself.

## 3. Isolation per platform, and the actual guarantee

- **Linux — strict, kernel-enforced for filesystem visibility.** Sealed actions run
  under `bubblewrap`. The namespace exposes the prepared `/work` tree, private
  `HOME`/`TMPDIR`, private `/dev/shm`, `/proc`, `/dev`, and declared toolchain roots
  only. Ordinary declared inputs are overmounted read-only, so writes to inputs fail
  instead of corrupting the CAS; explicitly writable inputs are private copies.
  Undeclared host files are **absent** from the namespace, so a read of one fails with
  `ENOENT`. The wrapper also drops effective capabilities, starts a new session,
  requires a user namespace, and sets UID/GID/supplementary groups to `1000`. Cgroup
  namespace isolation remains best-effort for host compatibility. Known non-hermetic
  surfaces such as kernel version, CPU count, `/proc/self/mountinfo`,
  `/proc/self/cgroup`, devices, and wall-clock time remain visible and are
  tested/documented as outside the filesystem visibility guarantee.
- **macOS — Seatbelt policy, not Linux namespaces.** Sealed actions run under a
  generated `sandbox-exec` profile. The profile denies network by default, clears and
  rebuilds the environment (§7.4), allows the prepared sandbox root plus private
  `HOME`/`TMPDIR`, allows declared toolchain roots read-only, and denies ordinary
  undeclared host file reads/writes. This is materially stronger than the old
  network-only profile, but it is still not Linux-style mount namespace isolation:
  denied paths may remain visible as metadata, a small Darwin runtime/system allowlist
  is visible, and read-only input enforcement comes from APFS clone/copy materialization
  plus permissions rather than read-only bind mounts. `sandbox-exec` is deprecated
  (still functional) with no public successor. For a hard guarantee, the **Linux-VM
  mode** remains the escape hatch.
- **No Windows in v1.**

Environment hermeticity *is* enforced on all platforms (the env is scrubbed to
canonical values; there is no host passthrough), independent of filesystem isolation.
For sealed/permeable actions, parent stdio is disconnected and inherited file
descriptors above stderr are marked close-on-exec before the sandbox backend starts;
native actions intentionally keep the host process environment and stdio behavior.

## 4. Undeclared inputs: isolation today, sensing later

Under-declaration is dangerous because an undeclared file is absent from the action identity.
If a process can still read it, a later change may not invalidate the cached result.

Current enforcement comes from the platform sandbox rather than a separate read tracer:

- **Linux:** undeclared paths are structurally absent from the Bubblewrap filesystem view, so
  reads fail. The current error may be the native tool's ordinary “file not found” rather than
  an Anneal diagnostic naming the missing declaration.
- **macOS:** the generated deny-by-default Seatbelt policy denies undeclared reads. This is a
  policy-interception boundary and is therefore graded `LoudBestEffort`, not Linux-equivalent
  `Enforced`.

Anneal does not currently have a general read-event tracer, an `anneal sense` command, or a
portable diagnostic that maps every denied open back to a BUILD attribute. The
[input-sensing proposal](../proposals/input-sensing.md) may add those authoring and diagnostic
capabilities without becoming execution-time truth.

### Why we do **not** use read-tracking to *relax* invalidation

The tempting inverse — "the tool reported it didn't read file `Y`, so don't invalidate
when `Y` changes" (discovered inputs / depfiles) — is rejected:

- it inverts our correctness bias toward **under**-invalidation (silently wrong),
  against the §1.4 invariant;
- to be *sound* it requires complete, trustworthy read observation — i.e. a strict
  sandbox we only have on Linux;
- and the benefit is small for our architecture: native tools are wrapped as opaque
  coarse engines (§3.2), and they already do fine-grained, depfile-aware incrementality
  internally (Cargo's fingerprints, etc.). An irrelevant-file edit re-invokes the inner
  tool, which then no-ops — a near-instant cost the batch-invocation optimization
  (§12.2) addresses more cheaply than a whole input-tracking subsystem would.

Therefore any future read observation is a *defensive* authoring/diagnostic tool, not a
performance authority that silently removes declared inputs. The performance case remains with
the wrapped native engines and explicit graph refinements.

## 5. Warm sandbox reuse — *implemented, on by default for owners*

> `warm.rs` and `run_warm` implement this path, and snapshot owners use it by default;
> `LocalExecutor::warm_reuse(false)` exists for verification and benchmarks. Cross-process
> workspace locking and representative Cargo cold-versus-warm neutrality tests have landed.
> Remaining hardening includes broader rule coverage and stronger workspace/package/target
> identity in the state key.

### 5.1 The reframe

Without an eligible warm directory, execution falls back to `fresh sandbox → materialize
sources → restore local snapshot → run → optionally save local snapshot → rm -rf`. The
`restore` and `rm -rf` are pure tax: we
reconstruct `target/` from the content-addressed store, use it, destroy it, and
reconstruct it again next time. Native cargo never does this — it leaves `target/` on
disk. **Warm reuse is the snapshot protocol with the snapshot kept *in place* instead of
round-tripped through the CAS.** The critical path collapses to `sync(O(change)) +
recompile(O(change))` ≈ native.

Because it is the snapshot protocol in another form, it inherits the **same correctness
invariant** (§1.4: warm output must equal cold output), guarded by the same verification
harness — with one *new* risk (dirty in-place state, §5.4).

### 5.2 Layering — a local accelerator *in front of* the CAS snapshot

Warm reuse does **not** replace the local CAS snapshot. The snapshot is still needed for
serving local snapshot **consumers** (test actions restore `target/` read-only into their own
fresh sandboxes) and for local fallback after a warm directory is absent. Snapshots are not
transported between machines. The fallback is:

1. **Warm dir present & valid** → reuse in place (fastest; the new path).
2. **No warm dir, CAS snapshot exists** → restore into a fresh dir, run, *keep it warm*.
   (the local fallback path, plus retention.)
3. **Neither** → cold build, then save snapshot + keep warm.

Shared snapshot owners still save to the local CAS for sibling consumers. Private owners may
skip that per-build save because the warm directory is their live copy. In both cases the warm
directory is a single-machine, single-key incremental accelerator.

### 5.3 Reusable **iff all** of

A warm dir is identified by its **`snapshot_key`** — `(toolchain, lockfile, triple,
profile/axes)`, deliberately **not** source content — so it is a valid cargo-incremental
base for *any* source state under the same key. The key is what's stable across
re-invocations and source edits but distinct per config: the same target rebuilt after an
edit reuses; a debug/release or toolchain/lockfile change does not. It is reusable iff:

- **Same `snapshot_key`.** A different key (toolchain bump, lockfile change, profile
  switch) maps to a different (or absent) warm dir, so wrong-key reuse never happens — no
  detection needed. *(Hardening: fold the package path into the key. Today
  `snapshot_key` omits target identity and relies on the lockfile — which lists workspace
  members — to distinguish workspaces; byte-identical workspace copies would otherwise
  collide. Pre-existing for the CAS snapshot too, but warm reuse makes it concrete.)*
- **The action is a snapshot *owner*** (`SnapshotBased`). Consumers (`SnapshotConsuming`
  test runs) keep their unique, fresh, restore-from-CAS sandboxes; they read the snapshot
  read-only and must not touch a mutable warm dir. The per-key stable path is exactly the
  `snap-K` path dropped from `sandbox_root` for parallelism, reintroduced *for owners
  only*.
- **Left clean** by the previous build (§5.4).
- **Holds the single-writer lock for its key** (§5.3.1).

#### 5.3.1 One warm dir per key — shared by a *group* of owners, serialized

A key subtlety: **several owner actions share one `snapshot_key`.** A `cargo_workspace`
at one config emits, all snapshotting the same `target/`: the `build` action, every unit
`test-compile`, every `doc`, every `integration`. They build into the **same logical
`target/`** — exactly as a developer's `cargo build && cargo test --no-run && cargo doc`
share one `target/` (test-compile reuses the rlibs `build` produced). So the warm dir is
**per key, shared by all same-key owners as a single-writer group — they serialize on it.**

This is *more correct and less total work* than the fresh-sandbox model it replaces: the
previous non-warm path had each same-key owner restore the snapshot into its own sandbox, run,
and **race to `save`** (last-writer-wins, so the stored snapshot is just whoever finished last, and each
redundantly recompiles shared dependencies). A shared serialized warm dir lets `target/`
**accumulate** across them as cargo intends — no redundant recompiles, no save race. The
cost is parallelism *among same-key actions* (one workspace's build + test-compiles + doc
can't overlap), which is fine: they were never truly parallel-incremental, cargo
parallelizes *within* each invocation, and **different keys (other workspaces/configs)
keep separate warm dirs and stay fully parallel.** A plain `anneal build` has a single
owner and no contention; the group only matters under `anneal test`.

### 5.4 Invalidation — two axes

- **Wrong world.** `snapshot_key` changed. Handled structurally by the key *being* the
  dir's identity (§5.3); no diffing.
- **Dirty state.** This is the one risk warm reuse adds over the CAS protocol: a CAS
  snapshot is only `save`d after a clean exit-0 build, so a restored snapshot is always a
  consistent post-success state, whereas an in-place dir can be left half-written by a
  crash, a timeout-kill, or a non-zero exit — at which point the on-disk source tree
  and/or `target/` no longer match the recorded manifest, and the next diff would compute
  wrong ops. Mitigation: a **commit record** governed by **clear-on-begin / set-on-commit**
  transaction semantics — delete it before mutating the dir, write it only after a fully
  successful build. On entry: **present → trust the manifest and reuse; absent →
  in-flight-or-crashed → discard** and fall back to tier 2 (re-restore the last good CAS
  snapshot) or tier 3 (cold). This is engine-agnostic (the same record protects a pnpm
  `node_modules` or `.next/cache` warm dir), so it lives at the **warm-dir root**, not
  inside `target/` — it attests the *whole dir's* consistency, independent of how `target/`
  got there (a tier-2 restore is clean without a build). It may carry the manifest digest
  ("clean as of input-set X"), which both pins the version and cross-checks the manifest;
  equivalently the two files collapse into one if `.anneal-inputs` itself is deleted-on-
  begin and atomically renamed-into-place on commit (presence = the commit bit).

Store cleanup and **eviction** remain open work (each warm directory is approximately one real
`target/`, so disk pressure must eventually garbage-collect them). No `anneal clean` command
exists today.

### 5.5 The sync — a delete/add/replace diff over declared inputs only

The warm dir holds *last* build's sources; the new build must see *this* build's. **The
diff runs once, at reuse time** (the first step of a build that reuses the sandbox) — not
continuously, and there is no filesystem watcher.

Two clarifications about *what* is compared and *where edits come from*:

- Edits originate in the **workspace** (the developer's repo), never in the sandbox. The
  warm dir at `.anneal/warm/<key>/` is Anneal-managed and only mutated by this sync; the
  developer never hand-edits it. The diff *propagates* a workspace edit into the sandbox.
- It is a **`path → content-digest` map comparison, not a byte/tree diff.** The *new* side
  is the freshly-hashed declared inputs that analysis computes anyway (for the action
  cache key), so it is ~free; the *old* side is the `.anneal-inputs` manifest written after
  the last build. A changed file is re-materialized *whole* (cargo recompiles at file
  granularity regardless).

We reconcile the new declared input set against the manifest, touching **only declared
input paths — never `target/`**:

| Manifest vs. new build | Action | Why |
|---|---|---|
| present, same digest | **leave untouched** | keeps old mtime → cargo fingerprint skips it |
| present, different digest | **re-materialize** | new content + fresh mtime → cargo recompiles |
| in new, absent from dir | **add** | new source file |
| in manifest, not in new | **delete** | a stale `.rs` left behind is a phantom compile — *correctness*, not tidiness |

The diff is O(changed files), from digests analysis already computed.

**The sharp edge is mtime — and an experiment confirmed it is a hard requirement, not a
nicety.** Cargo's freshness check is **mtime-based and content-blind** (verified, rust
1.95): given a warm `target/`, editing a source with a *fresh* mtime recompiles exactly
that crate (✓ the optimization works), but the *same content change behind a stale mtime
is silently skipped* — cargo keeps the wrong artifact. Holding content constant and
flipping only the mtime (stale→fresh) flips cargo skip→recompile, isolating mtime as the
sole trigger.

This is a **correctness** hazard, not just a perf one, and it has a concrete trigger: the
sync materializes from the CAS, and a blob's mtime is *whenever it was first `put`*.
New content → a new blob, mtime≈now (fine) — but **reverting a file to earlier content**
pulls an *old* blob (old mtime), so a plain clone/hardlink carries a stale mtime and cargo
misses the revert. Therefore:

- **Every file the sync writes because its content differs MUST be force-touched to a fresh
  mtime** (newer than the warm `target/`), regardless of the blob's own mtime. Unchanged
  files stay untouched so they keep old mtimes and cargo skips them.
- Because a Linux **hardlink shares the inode** (can't set a per-sandbox mtime without
  corrupting the shared CAS blob and every other sandbox sharing it), changed files in a
  warm dir need **distinct-inode placement** — macOS `clonefile` already gives it; Linux
  needs a **copy** for changed files, then `touch`.

(A future cargo `checksum`-based freshness mode would make cargo content-aware and retire
this hazard, but it is not the default and cannot be relied on.)

### 5.6 At rest — the warm dir holds *both* snapshot and code

A common first instinct is that a reusable sandbox at rest holds only the snapshot. It
holds **both** the materialized source tree and `target/` — and keeping the code is the
whole point:

```
.anneal/warm/<snapshot_key>/   ← the working tree — a FAITHFUL checkout, nothing extra
├── Cargo.toml, Cargo.lock, crate*/src/*.rs   ← declared inputs (source; mostly shared inodes/CoW; ~free)
├── target/ …                                 ← warm snapshot state (real bytes; the bulk)
└── .home/  .tmp/                             ← scratch (clearable)
.anneal/warm-meta/<snapshot_key>/   ← executor-only bookkeeping, kept OUT of the working tree
├── inputs                                    ← path→digest manifest (the §5.5 diff baseline)
└── committed                                 ← commit record — existence is the signal (§5.4)
```

The working tree should be **indistinguishable from a real checkout** — only source +
`target/` + scratch — so the native tool behaves exactly as it would locally. Anneal's
manifest and commit record are *our* bookkeeping, read by the **executor during sync**,
never by the rule's analysis (which globs the user's *repo*, not the warm dir), so they sit
**beside** the tree keyed by the same `snapshot_key`, not in it (hence no `.anneal-` hiding
prefix is needed). (Root-level dotfiles also work — cargo ignores them and the snapshot save
only touches `target/` — but the sibling layout keeps the "faithful checkout" property
crisp.) The commit record's **existence** is the whole signal (§5.4); equivalently it folds
into `inputs` via atomic-rename-on-commit, so "manifest present = committed."

**Why files, not an embedded database.** The commit dance is a transaction, so a DB
(SQLite/redb) is a fair instinct — but it's the wrong fit here, consistent with the rest of
Anneal's storage layer (CAS, action cache, and snapshot index are all flat-files +
atomic-rename). The load-bearing state (`target/`, source, CAS blobs) *must* be real files,
so a DB could only hold the small metadata sidecar — a **second source of truth** to keep
consistent with the filesystem that holds the real truth (DB says clean, dir was `rm`'d…).
And the only consistency we need is **one atomic op per key** — a manifest swap — which
`rename(2)` delivers crash-safely with zero dependencies; that's the right-sized OS
primitive, not a hand-rolled DB. A DB earns its keep for *relational/graph* metadata (cf.
Nix's SQLite for the store's reference graph + GC reachability); our per-key warm metadata
is flat and independent, so files win. (Even the eventual CAS GC is a file mark-and-sweep,
stop-the-world under the workspace lock — see TODO — so it doesn't force a DB either.)

**Declared inputs** are exactly the action's `inputs` map — the rule's enumerated source
set (the package tree globbed, minus `IGNORED_DIRS` like `target`/`.git`/`.anneal`) plus
any routed `data` — the *same* set the action cache key is computed over. `target/` is the
**snapshot, not an input** (it is kept warm, never diffed as source); declared outputs,
undeclared files, and the (ambient) toolchain are likewise outside the diff. So the
manifest is the per-path itemization of the set the cache key aggregates, and the sync's
universe is precisely `Action.inputs`.

If it held only `target/` and the code were re-laid every build, **every source file would
get a fresh mtime → cargo would see everything as newer than `target/` → full rebuild**,
defeating the optimization. The code must persist *in place* so unchanged files keep their
mtimes and only the genuine change is disturbed. So a warm sandbox is, by construction, an
ordinary cargo working directory that we sync deltas into. Contrast the at-rest forms:

- **Local CAS snapshot:** `target/` as content-addressed blobs + manifest — durable and
  deduplicated on one machine, but **must be reconstructed** to use.
- **Warm sandbox:** `target/` + source as a **ready-to-run directory** — zero
  reconstruction, but not shareable and not deduplicated.

Complementary, hence the §5.2 layering. One honest cost: a fresh sandbox wipes undeclared
writes every run; a warm dir accumulates them, slightly weakening the clean-slate
guarantee. Hermetic builds shouldn't write outside `target/` + declared outputs, but
build scripts sometimes do — track-and-clean, or treat it as part of the hermeticity
contract.

### 5.7 Payoff — measured

Warm reuse removes restore and teardown of a `target/`-sized working tree from the ordinary
warm critical path. Recorded measurements show that it materially reduces and bounds the
wrapper overhead, but does not make every incremental miss faster than native Cargo. The
methodology, current figures, and scenario-based performance contract live in
[`benchmarks/current.md`](../benchmarks/current.md); historical phase-by-phase investigations live
in the archived engineering log.

### 5.8 The residual: don't save *private* snapshots per build

After warm reuse, the warm critical path is `sync(O(change)) + recompile(O(change)) +
save(O(full target/), synchronous)`. The save (`SnapshotStore::save`) **re-walks all of
`target/` and re-reads+re-hashes every file every build** — O(`target/`) regardless of how
little changed — and sits on the critical path. It is the last term that both *dominates*
the residual and *scales* with `target/` size.

**The decoupling that licenses fixing it:** with warm reuse the producer reuses its
**in-place** `target/`, so it **never restores its own CAS snapshot**. The save's only
consumers are *external* — snapshot-*consuming* actions (test runs restoring `target/`),
other machines / CI, and cold-start after eviction.

#### 5.8.1 Private vs shared snapshots — the safe criterion

So the per-build CAS save is needed **only if the snapshot has consumers**. But "has a
consumer *in this graph*" is **not** a safe test, because the owner is action-cacheable:
build `//pkg` (owner runs, no consumer present → skip save → owner cached), then test
`//pkg` (owner *cache-hits*, never runs → the snapshot is never produced → the test's
restore finds nothing). The snapshot, once skipped-and-cached, is gone for good.

The safe criterion is "**is this snapshot *ever* consumed**" — a property of the rule's
intent, declared on the action, not derived per-graph:

- **Private** (`target/`): the owner's internal incremental state, **never** consumed
  (cargo's only "consumers" take content-addressed *outputs*/binaries, never `target/`).
  → **no per-build CAS save**; the warm dir is the live copy. Save only on eviction
  (snapshot-on-evict, future).
- **Shared** (`node_modules`): consumed by `SnapshotConsuming` actions (pnpm scripts).
  → **save every build** — a later cache-hit owner must still find the snapshot in the CAS.

Decided intrinsically: `save = SnapshotBased && snapshot_shared` (default **shared** —
conservative). `cargo_workspace` marks `target/` **private**; `pnpm_workspace`'s `install`
leaves `node_modules` **shared**. The warm-dir manifest (commit record) is written
regardless, so reuse works whether or not the CAS save runs.

This removes the O(`target/`) save from cargo's incremental path entirely — better than
the (still-useful, for *shared* snapshots) incremental + background save below.

#### 5.8.2 For shared snapshots: incremental + background save

- **Incremental save** — the prior manifest already records each file's `(mtime, size,
  mode, digest)`. Walk `target/`, `stat` each file; if `(mtime, size)` matches the prior
  manifest, **reuse its recorded digest** (no read, no hash); only changed/new files are
  read + hashed + `put`. Cost → O(change). This is cargo's own fingerprint trick applied to
  snapshot capture. (It trusts mtime+size — the *inverse* of §5.5: there we *force* fresh
  mtimes on synced inputs; here we *read* mtimes to detect changed outputs. A content change
  that didn't bump mtime would yield a stale snapshot for consumers; the §1.4 cold-vs-warm
  verification is the backstop, with a paranoid full-hash mode available.)
- **Background save** — return the build result immediately (the warm dir is already
  committed, so the next reuse doesn't wait) and run the save on a worker thread. The one
  sync point: a *consumer* needing the snapshot must wait for it — and `build_edges` already
  orders consumer-after-owner via the snapshot-owner edge, so "owner done" includes its save
  (or the save is joined lazily at restore). A plain `anneal build` with no consumers joins
  outstanding saves before process exit so nothing is lost.

**Why it matters:** together they reduce the user-felt warm critical path toward `sync +
recompile` and make incremental overhead less sensitive to `target/` size. The performance
contract is not that every miss beats native; see
[`benchmarks/current.md`](../benchmarks/current.md) for the current scenario-based gate.

### 5.9 Snapshots are universally non-portable — portability lives in the content-addressed layer

A snapshot is the **materialized mutable working state** of a tool's incremental cache
(`target/`, `node_modules`, `.next/cache`): tool-internal, often path- or platform-bound,
and **re-derivable**. The decision: **snapshots are local-only — never transported between
machines.** Cross-machine reuse lives entirely in the **content-addressed layer**: action
*outputs* (rlibs, build-script outputs — already path-independent) and each ecosystem's
*package store* (cargo's vendored deps / registry, pnpm's `.pnpm-store`). You **ship the
store, re-materialize the working tree locally** — never the working tree itself.

This is **safe by construction**: snapshots are correctness-*neutral* accelerators (§1.4),
so non-portability costs at most a re-derivation on a fresh machine, never a wrong result.
And it matches what mature CI converges on (cache the registry/store + a local build cache;
the *store* is the shared thing). Trying to make `target/` itself portable is the wrong
fight — it would need a fixed build-path convention *and* cross-machine build
reproducibility, fighting the tool's nature for a fragile payoff.

| ecosystem | local (snapshot, non-portable) | portable (content-addressed) |
|---|---|---|
| cargo | `target/` | vendored deps / registry + outputs (+ future rustc compilation cache) |
| pnpm  | `node_modules` | **`.pnpm-store`** (tarballs) + script outputs |

**Two axes, don't conflate them.** *Within-machine shared vs private* (§5.8.1 — does a
*local* sibling action restore it) is orthogonal to *cross-machine portable vs not* (this
section). `node_modules` is within-machine **shared** (local script consumers restore it →
saved to the *local* CAS each build) yet cross-machine **non-portable** (re-materialized per
machine). Both axes for `node_modules` say "don't transport the working tree."

**Consequence — lift `.pnpm-store` out of the snapshot.** Today the pnpm rule bundles
`node_modules` *and* `.pnpm-store` into one snapshot (`snapshot_paths = [node_modules,
.pnpm-store]`). The store is the *portable* half trapped inside a (now explicitly
non-portable) snapshot. It should be a **separate, portable, content-addressed cache**, so
cross-machine pnpm is "ship the store → `pnpm install --offline` re-links a local
`node_modules`" — symmetric with cargo's "ship vendor/registry → build locally." (Tracked.)

**Honest cargo caveat.** With snapshots local-only *and* cargo's build action coarse
(whole-workspace inputs), cross-machine cargo still pays a full `cargo build --workspace` on
*any* change — the action cache is workspace-granular, so it only exact-hits on an unchanged
workspace. Fine-grained cross-machine cargo reuse therefore needs a **rustc-level
compilation cache** (sccache-style, path-normalized) as the portable layer — the real v1.x
remote-cache piece. pnpm fares better: a warm `.pnpm-store` makes a fresh install fast.
