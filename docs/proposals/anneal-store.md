# The `anneal-store` crate

> **Status:** §8 phases 1–2 **implemented**; phase 3 (export/import/GC/fsck/doctor)
> remains proposed, gated on the remote-cache prerequisites.
> **Prerequisites:** none for the facade; the absorbed correctness fixes carry the
> Priority 0 items in [`TODO.md`](../../TODO.md) they close (listed in §7).

## 1. Motivation

Knowledge of the `.anneal/` directory is currently spread across four crates:

- `anneal-cas` owns `objects/` and the digest memo;
- `anneal-snapshot` owns the snapshot index;
- `anneal-exec` owns the path wiring itself (`LocalExecutor::new` joins every
  subdirectory), the action cache, warm manifests, and the materialized db;
- `anneal-cli` owns the workspace lock — the store's concurrency contract lives in the
  outermost layer.

No crate owns the layout as a whole, the crash-recovery policy, or the future
transport/garbage-collection boundary. `anneal-store` is a facade crate that becomes the
only crate that knows a path under `.anneal/` or a policy about trusting its contents.
It is a deep module one level above the existing deep modules (`anneal-cas`,
`anneal-snapshot`), which remain underneath it as implementation-detail crates.

## 2. Layout

```text
.anneal/
  store/                  # the transport unit — everything content-addressed
    format                # layout-version marker (import gate)
    objects/              # CAS blobs (from cas/objects)
    actions/              # action-cache entries (from cache/)
    snapshots/            # snapshot index (manifests and contents are blobs)
  local/                  # never leaves this machine
    digest-cache          # (path, mtime, size) → digest memo (moved out of cas/)
    warm/<key16>/         # warm working trees
    warm-meta/<key16>/    # input manifests (the warm commit records)
    sandboxes/            # transient per-action scratch
    queries/              # per-query stable execution roots (scratch)
    materialized/         # worktree materialization ownership
    tmp/                  # import staging and crash debris
  lock                    # the single immortal advisory lock (flock)
```

Placement rule: **if it is addressed by content, it goes in `store/`; if it references
mtimes, absolute paths, PIDs, or machine identity, it goes in `local/`.** Equivalently:
`store/` is money — portable, and losing it costs only time; `local/` is memory — bound
to this machine and wrong the moment it moves.

The two manifest families are the pair most easily confused: `warm-meta` describes the
declared **inputs** — what anneal placed into the tree; `snapshots` describes the tool's
mutable **state** — what the tool built there. The former is the working baseline and
commit record; the latter is the persisted generation restored on cold starts. Relatedly,
`warm-meta` sits beside `warm/` rather than inside it because the record vouches for the
tree: it must be unreachable from the process running inside the tree, which cannot
forge or destroy its own alibi.

Migration from the current flat layout: **none.** The pre-cutover layout is abandoned
with no handling code of any kind — a breaking change explicitly accepted (all state is
derived; users delete `.anneal` once or let the new layout replace it). `lock.rs` moves
out of `anneal-cli`.

## 3. Crash-safety model

The two halves of `.anneal` achieve safety by different mechanisms, and the distinction
is the point:

- `store/` is safe **by ordering**: every process-crash state is already valid.
- `local/` is safe **by detect-and-degrade**: every crash state must reconcile to a valid
  warm baseline or fall back to cold, never propagate to outputs (the §1.4/§8.2
  neutrality invariant is the backstop).

### 3.1 `store/` invariants

1. **Per-file atomicity.** Every write is temp-file + rename, with pid/nonce temp names
   (the snapshot index's fixed `.tmp` name is a defect this crate absorbs).
2. **Back-to-front reference ordering.** A pointer is never written before its pointee:
   output blobs before the action entry; snapshot file blobs, then the manifest blob,
   then the index. A crash leaves unreferenced debris (GC food) or a complete older
   generation — never a dangling pointer, in the pre-GC world.
3. **Idempotent, commutative writes.** Same name implies same bytes; racing identical
   writers are tolerated as success.

No journal or embedded database is used, and none is needed: the store has no
multi-file transaction whose intermediate states are meaningless. A journal answers
"what was in progress?" — content addressing answers "is what is here valid?" locally,
per file. The one place a journal-shaped record *is* required — mutable,
non-content-addressed state — already has one (§3.2).

**Power loss** is the one exception to process-crash safety: rename is atomic, not
durable, so a durable name can wrap undurable data (a "lying name" — the filename is the
SHA-256 of content the file no longer holds). Detection, not prevention: existence-check
on every action-cache hit; hash verification on import (the trust boundary) and as an
opt-in read tier; `fsync` only behind an explicit paranoid-durability flag; a `fsck`
walker as the offline auditor. Worst case is always re-execution, never a purge.

### 3.2 `local/` recovery

- **Warm trees** use the manifest-as-commit-record protocol: reuse only when a committed
  manifest exists and the tree is present; BEGIN removes the record *before* any
  mutation; COMMIT writes it atomically after success only; absence means "unproven"
  and cold-populates (wipe, materialize, restore the last snapshot). A failed build
  deliberately leaves no commit.
- **Degrade-vs-fail policy is encoded, not ad hoc:** accelerator state inside `.anneal`
  degrades — a torn warm manifest or snapshot index is treated as absent (cold path),
  never as a build error; state that guards user worktree bytes fails closed — the
  materialized db refuses to touch files it cannot prove it owns (never-clobber), with a
  documented rebuild path.
- **Warm drift:** the input manifest records `(mtime, size)` per entry and reuse
  stat-verifies unchanged inputs, re-placing on mismatch. A re-placed file gets a fresh
  mtime, so the native tool rebuilds exactly what depended on it — the check self-heals.
- **Boot recovery on first `lock()`:** sweep orphaned `sandboxes/` and `tmp/` debris;
  load all manifests through the tolerant path. (`Store::open` itself is read-only —
  only the guard mutates, so lock-free readers never trigger migration or sweeps.)

### 3.3 How this is enforced

A debug crash-injection hook (`ANNEAL_CRASH_AFTER=<phase>`) plus table-driven process
tests assert one sentence for every labeled phase: *the next run produces identical
declared outputs, whether it recovers warm or falls back to cold.* The minimum label
set: `blob-put`, `action-insert`, `snapshot-manifest`, `snapshot-index`, `warm-begin`,
`warm-input-place`, `warm-commit`, `materialize-write`. The BEGIN/COMMIT
ordering, the blob-before-entry ordering, and the boot sweeps are pinned by these tests,
not by review.

## 4. Concurrency model

- **Readers are lock-free at both levels.** Everything readable without a lock is
  immutable-once-published (atomic rename), and readers treat absence as "recompute or
  report" (fail-open), never as an error. This is the contract `affected`/`why` already
  live by.
- **Mutators serialize on one immortal lock.** `flock(LOCK_EX)` on `.anneal/lock`, never
  deleted, never inside a GC-able zone. All writes go through it, including the ones
  individually safe today, so no caller ever needs to know which writes are safe
  unsynchronized. Bulk operations (GC, import, format migration) hold it too. A second
  `lock()` in the same process is a defined error, not a deadlock.
- **In-process granularity lives under the coarse lock.** Same-warm-key owners serialize
  on a per-key mutex acquired by an RAII warm handle; different keys run free on the
  executor's threads. In-process locks are sound *because* the flock exists — each
  process's mutexes see only that process. The in-process hierarchy stays shallow
  (guard → warm handle) and is therefore deadlock-free by construction.
- **No per-key lock files.** A lock file GC could delete breaks flock mutual exclusion
  (two processes holding the "same" lock on orphaned inodes). If concurrency demand ever
  justifies finer cross-process locking, the first cut is the `store/`-vs-workspace seam
  (natural under store redirection), and per-warm-key lock files are the second — with
  lock-file immortality as a hard invariant.
- **Deletion is reader-safe by grace period, not exclusion.** GC condemns in one
  generation and unlinks in a later one (or after an mtime threshold), so the
  has→open race against a lock-free reader vanishes for any realistic reader; the
  fail-open reader covers the remainder. Format migration rewrites live entries by
  blue/green layout swap (write the new tree, flip a pointer atomically), which is
  atomic *to readers* rather than exclusive of them.

## 5. Public API

The API makes the safe behavior the easy behavior — policy is carried by types:

```rust
let store = Store::open(&workspace_root)?;       // read-only: resolve the layout

let hit  = store.actions().lookup(&key);         // lock-free read view
let blob = store.cas().get(&digest);

let guard = store.lock()?;                        // migration + boot recovery, then
guard.actions().insert(&key, &result)?;           // the write capability (shared-ref)
let warm  = guard.warm(&key);                     // RAII per-key transaction handle
warm.begin(); warm.sync(&desired)?; warm.commit()?;

store.export(&path, ExportOpts::default())?;      // the transport boundary is the
store.import(&path, Verify::Hash)?;               // store/ subtree, nothing else
store.gc(Policy::default()); store.fsck();        // later phases
```

- **`Store`** is the read view: only immutable-once-published data is reachable from it.
- **`WorkspaceGuard<'_>`** is the write capability, borrowed, `Send + Sync`, RAII over
  the flock. Mutating methods take `&WorkspaceGuard` (shared) so the executor's thread
  parallelism is preserved.
- **`Recovered<T>`** — `Clean(T) | Degraded { value, note } | Absent` — is the return
  type for every manifest/index load, making "torn ⇒ degrade" inescapable.
- **`Verify::Stats | Verify::Hash`** names the trust tier at every verification boundary
  (reuse checks, import admission).

Dependency direction: `anneal-store → anneal-cas, anneal-snapshot, anneal-core`;
`anneal-exec → anneal-store`. Identity *computation* stays beside `Action` in
`anneal-exec`; the store exposes digest-keyed persistence. Warm diff-sync logic stays in
`warm.rs`; the store owns manifest load/save and the commit-record semantics. The test
for what belongs in the crate: does it know a path under `.anneal/` or a policy about
trusting `.anneal/` contents? If yes, it is store code; if it is about *what to build*,
it is not.

## 6. Transport boundary

Export and import move exactly the `store/` subtree — never `local/`, never the lock,
never `digest-cache`. The export embeds the `format` marker so incompatible layouts are
rejected on import. Two versioning layers stay distinct: the folder marker gates *where
files live*; the in-key tags (`anneal-action-v1`, `SANDBOX_VERSION`) gate *what entries
mean*. Imports verify blob digests, require provenance lines, and honor the enforcement
floor. Mark-and-sweep export (entries touched this run plus the imported set) keeps
exports incremental. This boundary is also the local half of the
[remote cache proposal](remote-cache.md); a remote backend substitutes for the archive
without changing it.

## 7. Absorbed correctness work

Implementing the crate closes, or gives a home to, the store-side audit:

- identity completeness — the declared output map enters `action_digest` via an explicit
  `ActionIdentity` struct whose compiler-enforced field set is the hash input; the struct
  lives beside `Action` in `anneal-exec`, and the store hashes the canonical bytes it is
  handed ([TODO.md](../../TODO.md) P0 #1, #6);
- explicit generic-action cacheability policy (P0 #3, resolved: arbitrary commands are
  `NonCacheable` by default and cache only through explicit opt-in — a fixed-output pin
  or a declared-deterministic path with audit sampling);
- owner identity in snapshot/warm keys, plus a human-readable owner line in the warm
  manifest for diagnostics (P0 #4; re-keying invalidates existing warm dirs and snapshots
  once — acceptable for accelerators);
- digest-memo hardening — `(mtime, size, ctime, inode)` with a paranoid re-hash tier
  (P0 #2);
- action-hit existence check (fail-open to re-run, as the query path already does);
- tolerant manifest/index loads and boot-time debris sweeps (§3.2);
- warm-input stat-verification (§3.2);
- pid/nonce temp names everywhere (snapshot index today);
- the crash-injection test harness (P1 interruption coverage).

## 8. Rollout

1. **Facade, no behavior change:** layout constants, `Store::open` returning the
   existing handles, `lock.rs` relocated, legacy-layout migration, boot recovery.
2. **Persistence and policy:** action-cache and warm-manifest persistence move in with
   `Recovered`/`Verify`; the §7 fixes land inside the crate, each with its
   crash-injection test.
3. **Lifecycle:** `export`/`import`, GC, `fsck`, `doctor`. Import/export ship only after
   the identity items they transport are closed; until then the boundary exists but is
   exercised only by tests.

## 9. Resolved implementation decisions

1. **First pass scope: phases 1 and 2** — facade cutover, persistence and policy types,
   all §7 fixes, crash-injection tests. Phase 3 (export/import/GC/fsck/doctor) is a
   follow-up gated on the identity work it transports.
2. **Generic-action cacheability:** `NonCacheable` by default; explicit opt-in only.
3. **Legacy layout:** no migration code; breaking change accepted.
4. **Interruption tests:** store-API fault injection for every §3.3 phase label, plus a
   small set of end-to-end subprocess-kill tests on the Linux CI lane.
5. **Consequences accepted:** identity changes (output map, owner-identity keys, the
   `anneal-action-v1` → `v2` bump) and the layout change invalidate every existing cache
   entry, snapshot, and warm dir, exactly once.

## 10. Deliberately excluded

No embedded database or journal (§3.1); no per-key lock files (§4); no remote backend
(this proposal defines the local boundary the [remote cache](remote-cache.md) will
reuse); no change to rule semantics, executor scheduling, or the CLI beyond acquiring
the store through `Store::open`. The current product is unaffected until phase 1 lands;
`affected`/`why` remain lock-free reads.
