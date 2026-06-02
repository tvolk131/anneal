# Phase 0 spike findings

Throwaway code under `spikes/`. Purpose: retire the two highest-risk §22 assumptions
before committing real interfaces. Run with:

```
nix develop --command cargo run -p starlark-spike
nix develop --command cargo run -p materializer-spike --release
```

Toolchain at time of writing: rustc 1.95, starlark 0.14.0, sha2 0.11, macOS (APFS).

---

## Spike A — starlark-rust integration  ✅ assumption holds

**Question (§22):** does starlark-rust support custom error messages, source-location
preservation, and registering rule primitives as globals — cleanly enough to underpin
the entire BUILD layer (§4, §17.1)?

**Result: yes, on all four checks.**

1. **Load + evaluate** a BUILD-shaped file via `AstModule::parse` + `Evaluator::eval_module`.
2. **Rule primitives as globals** — `#[starlark_module]` functions registered via
   `GlobalsBuilder`, writing into a per-evaluation sink reached through `eval.extra`
   (a `&dyn AnyLifetime` downcast to our `ProvidesStaticType` type). This is exactly the
   shape the real target-graph builder needs.
3. **Source locations** — an undefined-symbol error renders compiler-style and exposes
   the span programmatically:
   ```
   error: Variable `undefined_variable` not found
    --> crates/BUILD:3:24
   -> programmatic span: crates/BUILD:3:24-42
   ```
4. **Custom rule-boundary errors** — an `anyhow::anyhow!` returned from a rule primitive
   surfaces with a traceback and a span pointing at the *call site in the user's BUILD
   file* (`crates/BUILD:2:1-27`), not at internal rule code. This is precisely §17.1's
   requirement.

**Constraints this imposes on the real `anneal-loader` interface:**

- **API pins for 0.14:** construct the module via `Module::with_temp_heap(|module| ...)`
  (no `Module::new()`); rule primitives return `anyhow::Result<_>`; the per-evaluation
  state type derives `ProvidesStaticType + Default` and is passed as `eval.extra`.
- `starlark::Error` carries `.span() -> Option<&FileSpan>` *and* a compiler-style
  `Display`. The loader should **keep the structured `FileSpan`** and feed it into
  `anneal-diagnostics` rather than rendering through starlark's `Display` — we want our
  own §17.1 formatting and stable codes (`MB….`), with starlark's spans as the source
  of location truth.
- The `eval.extra` sink pattern is single-threaded per evaluation (`RefCell`). Loading is
  "parallelized by package" (§3.1), so parallelism lives **across** `BUILD` evaluations
  (one sink each), never within one — matches the design.

**Not yet probed (defer to Phase 2, not blockers):** `load()` resolution across files;
monorepo-scale performance (§22's perf clause); freezing modules for caching; the
restricted-subset linter (§4.2). The core mechanism is proven; these are additive.

---

## Spike B — CAS + hardlink materializer on macOS/APFS  ✅ assumption holds (one perf flag)

**Question (§22):** is hardlink-from-CAS on APFS viable at scale — file locking, per-inode
hardlink limits, cross-filesystem fallback?

**Result: yes, with one performance item to benchmark in Phase 3.**

1. **Content addressing + dedup** — re-storing identical content yields the same digest
   and no second write (temp-file + atomic rename on store).
2. **Hardlink shares the inode** — materialized file and CAS blob report the *same inode*;
   it is a true hardlink, not a copy. Setup is O(1) per file with shared storage (§3.4).
3. **Per-inode hardlink limit is a non-issue** — created **50,000** hardlinks to a single
   inode with **no limit hit** (`nlink=50002`). APFS's ceiling is far above any realistic
   CAS dedup factor; a popular blob can be materialized into as many sandboxes as needed.
4. **Same-filesystem requirement** — verified via `st_dev` comparison; CAS and sandbox
   must share a volume (they do under `.anneal/`). The **EXDEV → copy fallback** path is
   implemented and detected by errno 18. (Couldn't force a cross-volume case in this
   environment — every probed path shared `st_dev` — but the detection + fallback code
   path is in place.)
5. **`sandbox-exec` present and functional** — ran a command under a trivial profile.
   Confirms the macOS `sealed`-mode isolation layer (§7.3) is available (still deprecated
   but functional, per §22's accepted limitation).

**Constraints / flags for the real `anneal-exec` + `anneal-cas`:**

- ⚠️ **Hardlink throughput, benchmark in Phase 3.** ~4,600 links/sec (~215 µs each) in this
  run — slower than a pure metadata op should be, likely TMPDIR/sandbox overhead. A build
  materializing thousands of inputs could spend seconds purely linking. *This is not the
  §22 limit concern (that's retired), but it feeds the §20 benchmark gates.* Action:
  measure on a real `.anneal/` volume; if material, consider batching / parallel
  materialization. Note the realistic pattern is 1 link each into many inodes, not 50k
  into one, so this number is a pessimistic stress figure.
- **CAS and sandbox roots must be co-located on one volume.** The materializer must assert
  `st_dev` equality at setup and fall back to copy (already coded) — and the installer /
  `init` should place `.anneal/cas` and sandbox roots on the same filesystem by
  construction (§3.4 "avoided by configuration").
- **Atomic store** via temp-file + rename is required (a concurrent build must never read a
  half-written blob) — confirmed working; keep it in the real CAS.

---

## Net effect on the plan

Both Phase 0 risks are **retired**. Two items carried forward, neither a blocker:
- starlark perf at monorepo scale → revisit in Phase 2 (§22 perf clause).
- hardlink throughput → measure in Phase 3, feeds §20 benchmark gates.
