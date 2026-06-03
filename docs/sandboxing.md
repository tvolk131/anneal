# Sandboxing, materialization, and input hermeticity

> Companion to `build-system-design.md` (§3.4, §7.3, §22). The main doc states the
> hermeticity *principles*; this note collects the *mechanics* — how inputs are
> materialized, how isolation is enforced on each platform, what correctness
> guarantee each platform actually provides, and how read-tracking is used
> **defensively** to catch under-declared inputs.

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
| **Linux** | hardlink from the CAS (shared inode) | read-only enforced by the sandbox's read-only bind mounts |
| **macOS / APFS** | `clonefile` (copy-on-write, **distinct inode**) + `chmod 0444` | a write COWs — the store blob is never mutated; read-only is safe to set because it's a separate inode; per-inode hardlink limit sidestepped |
| any, cross-volume | copy + `0444` | (fallback) |

The macOS choice is deliberate: a hardlink shares the inode, so a misbehaving action
that wrote to a materialized input would mutate the immutable store. A CoW clone makes
that impossible (proven by a test that clears the read-only bit, overwrites the input,
and confirms the store blob is intact).

Output handling: parent directories for declared output paths are pre-created, so an
action can write a nested output (`gen/config.json`) without `mkdir`-ing itself.

## 3. Isolation per platform, and the actual guarantee

- **Linux — strict, kernel-enforced.** Mount namespaces with read-only bind mounts of
  *only* the declared inputs. Undeclared files are **absent** from the namespace, so a
  read of one fails with `ENOENT`. Hermeticity is guaranteed *by construction*.
- **macOS — best-effort (~95%).** `sandbox-exec` profiles. Our sealed profile denies
  network; environment is cleared and reset to canonical values (§7.4). But the
  filesystem is **not** strictly isolated — an action can read undeclared host files
  and succeed. `sandbox-exec` is deprecated (still functional) with no public
  successor. For a hard guarantee, the **Linux-VM mode** is the escape hatch.
- **No Windows in v1.**

Environment hermeticity *is* enforced on all platforms (the env is scrubbed to
canonical values; there is no host passthrough), independent of filesystem isolation.

## 4. Input hermeticity via read-tracking — defensive, never permissive

A separate concern from isolation: catching **under-declaration** — an action that
reads a file it never declared. Under-declaration is dangerous because the undeclared
file is absent from the action's cache key, so a change to it won't invalidate →
**silently stale/wrong output**, or "works on my machine, breaks on a clean/remote
build."

The chosen posture: read-tracking is used **to enforce declarations, never to relax
caching.** It converts a silent correctness bug into a loud build-time failure — the
most valuable kind of check, and aligned with the §1.4 correctness-neutral invariant
("fail loudly rather than cache wrongly").

What it buys per platform:

- **Linux:** hermeticity is *already* guaranteed by isolation (undeclared reads fail
  with `ENOENT`). Read-tracking mainly improves the **diagnostic** — turning a cryptic
  "file not found" into "you read undeclared file `X`; declare it." It does not
  strengthen the guarantee, which is structural.
- **macOS:** this is where it earns its keep. Without it, under-declaration is
  **silent** (best-effort isolation lets host reads succeed). Read-tracking — via a
  deny-by-default `file-read` profile, or by observing opens — turns silent
  under-declaration into a caught/failed build, **raising the floor from "silent holes"
  to "violations fail loudly."** The tracking itself is best-effort on macOS, so it is
  a *mitigation*, not a hard guarantee; the Linux-VM mode remains the path for hard
  guarantees.

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

So read-tracking is a *defensive* tool here (catch missing declarations), not a
*performance* tool (skip work). The performance case is outsourced to the inner engines
by the wrap-don't-decompose architecture.

## 5. Warm sandbox reuse — *implemented, on by default for owners*

> Motivated by the §20 benchmark: on the canonical **incremental** case (edit one crate,
> rebuild), the fresh-sandbox model loses to native cargo and loses *worse* as the
> workspace grows (+50% at 4 crates → +265% at 64; +3495% on a real `syn` workload, where
> the restored `target/` lands at a new path each run and cargo full-rebuilds). This is
> **implemented** (`warm.rs` engine + `run_warm`) and is now the **default** for snapshot
> owners (`LocalExecutor::warm_reuse(false)` opts out for verification/benchmarks). It is
> the sandbox-lifecycle counterpart to the snapshot protocol (`build-system-design.md` §8.2).
>
> **Default-on prerequisites (tracked, not yet closed):** (1) routine cold-vs-warm
> correctness-neutrality verification (§1.4) — the isolation model is now persistent
> in-place, not fresh-per-build; (2) the cross-process workspace lock — persistent per-key
> dirs are shared across concurrent `anneal` processes (today's per-key lock is in-process).
>
> **What default-on does and doesn't fix:** it eliminates the +3495% on the *same-machine*
> dev loop (owners reuse the warm dir at a stable path → true incremental). It does **not**
> make cargo's `target/` path-relocatable — the full-rebuild cost relocates to cases with no
> at-path warm dir: **cross-machine / fresh CI** (needs warm-*dir* caching across runs, or
> path canonicalization + a shared snapshot) and **post-eviction** (one-shot; → snapshot-on-
> evict). Those are one-time, not per-build.

### 5.1 The reframe

Every build today does `fresh sandbox → materialize sources → restore target/ (from CAS)
→ run → save target/ (to CAS) → rm -rf`. The `restore` and `rm -rf` are pure tax: we
reconstruct `target/` from the content-addressed store, use it, destroy it, and
reconstruct it again next time. Native cargo never does this — it leaves `target/` on
disk. **Warm reuse is the snapshot protocol with the snapshot kept *in place* instead of
round-tripped through the CAS.** The critical path collapses to `sync(O(change)) +
recompile(O(change))` ≈ native.

Because it is the snapshot protocol in another form, it inherits the **same correctness
invariant** (§1.4: warm output must equal cold output), guarded by the same verification
harness — with one *new* risk (dirty in-place state, §5.4).

### 5.2 Layering — a local accelerator *in front of* the CAS snapshot

Warm reuse does **not** replace the CAS snapshot. The CAS snapshot is still needed for
what the warm dir cannot do: serving snapshot **consumers** (test actions restore
`target/` read-only into their own fresh sandboxes), **cross-machine sharing**, and **CI
cold-start**. So a three-tier fallback:

1. **Warm dir present & valid** → reuse in place (fastest; the new path).
2. **No warm dir, CAS snapshot exists** → restore into a fresh dir, run, *keep it warm*.
   (today's behavior, plus retention.)
3. **Neither** → cold build, then save snapshot + keep warm.

The owner still saves to CAS — but that save can move **off the critical path**
(background, and incremental: re-`put` only changed files). The warm dir is purely a
single-machine, single-key incremental accelerator.

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

This is *more correct and less total work* than the fresh-sandbox model it replaces: today
each same-key owner restores the snapshot into its own sandbox, runs, and **races to
`save`** (last-writer-wins, so the stored snapshot is just whoever finished last, and each
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

Plus the usual: explicit `anneal clean`, and **eviction** (each warm dir is ≈ one real
`target/`, so disk pressure must GC them — ties into the eviction work).

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

- **CAS snapshot** (today): `target/` as content-addressed blobs + manifest — durable,
  deduplicated, shareable, but **must be reconstructed** to use.
- **Warm sandbox** (proposed): `target/` + source as a **ready-to-run directory** — zero
  reconstruction, but not shareable and not deduplicated.

Complementary, hence the §5.2 layering. One honest cost: a fresh sandbox wipes undeclared
writes every run; a warm dir accumulates them, slightly weakening the clean-slate
guarantee. Hermetic builds shouldn't write outside `target/` + declared outputs, but
build scripts sometimes do — track-and-clean, or treat it as part of the hermeticity
contract.

### 5.7 Payoff — measured

Warm reuse removes restore + teardown (the O(`target/`) terms) from the critical path. On
the single-package-change benchmark vs native cargo:

| | non-warm | **warm reuse** |
|---|---|---|
| N=16 | +91% | **+36%** |
| N=48 | +203% | **+58%** |

The non-warm overhead **diverges** with workspace size; warm reuse is **bounded** — exactly
the restore+teardown removal. The residual (+36–58%) is dominated by the snapshot save,
still synchronous and full — §5.8. On trivial crates Anneal does not yet *beat* native
(whose incremental is ≈ its fixed startup, with nothing to amortize against); that verdict
needs a realistic workload where compile time dominates.

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
  (snapshot-on-evict, future) or explicit cross-machine publish.
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

**Why it matters:** together they reduce the user-felt warm critical path to `sync +
recompile` ≈ native incremental, and make the incremental overhead **independent of
`target/` size** — killing the scaling that made non-warm diverge. This is the piece that
moves the §20.3 "incremental must *beat*" gate from "match" toward "beat".

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
