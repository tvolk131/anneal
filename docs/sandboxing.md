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
