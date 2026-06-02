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

## 5. Warm sandbox reuse — *designed, not yet built*

> Motivated by the §20 benchmark: on the canonical **incremental** case (edit one crate,
> rebuild), the current fresh-sandbox model loses to native cargo and loses *worse* as the
> workspace grows (+50% at 4 crates → +265% at 64). The phase breakdown shows why — see
> the end of this section. This is the design for closing that gap. It is the
> sandbox-lifecycle counterpart to the snapshot protocol (`build-system-design.md` §8.2).

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
base for *any* source state under the same key. It is reusable iff:

- **Same `snapshot_key`.** A different key (toolchain bump, lockfile change, profile
  switch) maps to a different (or absent) warm dir, so wrong-key reuse never happens — no
  detection needed.
- **The action is a snapshot *owner*** (`SnapshotBased`). Consumers (`SnapshotConsuming`
  test runs) keep their unique, fresh, restore-from-CAS sandboxes; they read the snapshot
  read-only and must not touch the owner's mutable warm dir. This is the reconciliation
  with parallel execution: **owners reuse (one per key, naturally serialized); consumers
  stay unique and parallel** — the per-key stable path is exactly the `snap-K` path that
  was dropped from `sandbox_root` for parallelism, reintroduced *for owners only*.
- **Left clean** by the previous build (§5.4).
- **Not concurrently held** (single-writer lock per key — cheap: one owner per key per
  graph, and identical-config builds wouldn't run concurrently anyway).

### 5.4 Invalidation — two axes

- **Wrong world.** `snapshot_key` changed. Handled structurally by the key *being* the
  dir's identity (§5.3); no diffing.
- **Dirty state.** This is the one risk warm reuse adds over the CAS protocol: a CAS
  snapshot is only `save`d after a clean exit-0 build, so a restored snapshot is always a
  consistent post-success state, whereas an in-place dir can be left half-written by a
  crash, a timeout-kill, or a non-zero exit. Mitigation: a **clean-commit marker** written
  only after success; on entry, a missing/stale marker means "untrustworthy" → fall back
  to tier 2 (re-restore the last good CAS snapshot) or tier 3 (cold).

Plus the usual: explicit `anneal clean`, and **eviction** (each warm dir is ≈ one real
`target/`, so disk pressure must GC them — ties into the eviction work).

### 5.5 The sync — a delete/add/replace diff over declared inputs only

The warm dir holds *last* build's sources; the new build must see *this* build's. We
reconcile the new **declared input set** against the recorded `.anneal-inputs` manifest
(`path → content-digest`), touching **only declared input paths — never `target/`**:

| Manifest vs. new build | Action | Why |
|---|---|---|
| present, same digest | **leave untouched** | keeps old mtime → cargo fingerprint skips it |
| present, different digest | **re-materialize** | new content + fresh mtime → cargo recompiles |
| in new, absent from dir | **add** | new source file |
| in manifest, not in new | **delete** | a stale `.rs` left behind is a phantom compile — *correctness*, not tidiness |

The diff is O(changed files), from digests analysis already computed.

**The sharp edge is mtime — validate empirically before building.** Cargo's fingerprints
are mtime-sensitive: a changed file must end up **newer** than the `target/` artifacts (so
cargo recompiles it), an unchanged file must keep its mtime (so cargo skips it). The
wrinkle is materialization (§2): a Linux **hardlink shares the inode**, so we cannot set a
per-sandbox mtime without corrupting the shared CAS blob's mtime and every other sandbox
sharing it. So changed-file sync needs **distinct-inode placement** — macOS `clonefile`
already gives it; Linux needs a copy (or a post-link `touch`). Getting this wrong means
cargo either silently skips a changed file (a correctness bug) or rebuilds everything (no
speedup).

### 5.6 At rest — the warm dir holds *both* snapshot and code

A common first instinct is that a reusable sandbox at rest holds only the snapshot. It
holds **both** the materialized source tree and `target/` — and keeping the code is the
whole point:

```
.anneal/warm/<snapshot_key>/
├── Cargo.toml, Cargo.lock, crate*/src/*.rs   ← source (mostly shared inodes/CoW; ~free)
├── target/ …                                 ← warm build state (real bytes; the bulk)
├── .home/  .tmp/                             ← scratch (clearable)
├── .anneal-inputs                            ← path→digest manifest, for §5.5's diff
└── target/.anneal-committed                  ← clean-commit marker (§5.4)
```

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

### 5.7 Expected payoff

From the measured incremental build @ 16 crates (~100 ms Anneal vs ~50 ms native):

| Removed by | Phase | ~cost @ N=16 |
|---|---|---|
| warm reuse | restore snapshot | 16 ms |
| warm reuse | teardown sandbox | 9 ms |
| background + incremental save | save snapshot | 7 ms |
| warm reuse (stable mtimes) | run-overhead vs native | ~12 ms |

The plausible end state: incremental **≈ native**, with the CAS snapshot demoted to a
background durability/sharing layer — turning the §20.3 "incremental must *beat*" gate from
"lose, diverging" into "match-or-beat," with Anneal's added value (cross-machine cache,
affected selection) sitting on top of parity rather than fighting a deficit.
