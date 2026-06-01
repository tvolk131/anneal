# `pnpm_workspace` — scope and design

> Companion to `build-system-design.md` (§13.4 *rule mechanics*, §13.6 *axis matrix*,
> §14.1–14.6 *generated native packages and routing*, §14.5 *cacheability modes*) and to
> `docs/rules.md` (the rule model this design stresses). Captures the Milestone-1 scope for
> the second native ecosystem and the §14.3 Nickel → TypeScript routing demo, plus the
> design reasoning behind every non-obvious choice.

## 1. Why pnpm is the hard rule

`cargo_workspace` could be automatic because Rust is rigidly structured (`docs/rules.md` §3):
the rule knows what `cargo build` produces and what `target/` is. **pnpm has almost none of
that.** It is a *package manager + script runner*, not a toolchain:

- `pnpm build` / `pnpm test` mean nothing intrinsic — they run arbitrary `package.json`
  scripts (tsc, vite, esbuild, next, vitest, jest, `node:test`, or *nothing*).
- The output set of a build script is **opaque** — `dist/`? `.next/`? nothing? The rule
  cannot infer it from structure, because there is no fixed structure.
- The cache boundary is split: pnpm owns **resolution + install**; *build* caching belongs
  to whatever inner tool the script shells out to.

So `pnpm_workspace` sits in the *middle* of the inference↔declaration spectrum. It splits
into two layers, and only one is pnpm's:

- **The install layer — deterministic, pnpm-owned, inferred.** Resolution + install
  (lockfile → store → `node_modules`). Exactly analogous to Cargo's resolution; the rule
  automates it fully.
- **The script layer — open-ended, user-declared.** `pnpm run <script>`. The rule cannot
  infer inputs/outputs/cacheability, so the user declares which scripts to run and what kind
  each is.

> **Caveat on "deterministic install":** the genuinely deterministic core is *resolution*.
> `postinstall`/`prepare` lifecycle scripts run arbitrary code (network, codegen, native
> compilation) and live in the same sealed-or-uncached bucket as any other script. Install
> is treated as platform-sensitive and snapshot-backed, not assumed reproducible.

## 2. The rule at a glance

```python
pnpm_workspace(
    name = "app",
    data = [("//app:cfg", "@gen/config")],   # routed Nickel package, name-resolved (§4)
    scripts = {
        "test":  { kind = "test" },                       # result captured; coverage axis
        "build": { kind = "build", outputs = ["dist"] },  # artifact → provider; minify axis
    },
)
```

Actions emitted:

- **`install`** — `pnpm install --offline --frozen-lockfile`. Cached (action cache) +
  snapshot (`node_modules`, store; keyed and justified in §6). The deterministic, inferred
  core. Mirrors Cargo's `--offline --locked` posture: external-dependency *vendoring is
  deferred*; the M1 demo uses zero registry dependencies (it consumes only the local generated
  package), so an offline install against an empty/local store succeeds even sealed.
- **one action per declared script**, each a true dependent of `install` via an edge carrying
  the install-snapshot identity (§6), tagged with an explicit `kind` (§3). Default
  **non-cacheable + snapshot-accelerated** (§5).

Scripts are **surfaced by discovery but never auto-run** — `dev`/`start` never terminate,
`clean` is destructive. Discovery means "the rule reads `package.json` `scripts` at analysis
(static — allowed) and lets the user reference them"; *which* scripts become actions, and as
what kind, is always the user's explicit declaration.

## 3. Test vs. build are different action kinds

Mechanically both "run a script," but they diverge, so `kind` is **explicit** (no
`test`→test name convention — a `spec`/`check` script silently misclassifying is exactly the
failure class we avoid):

| | **`kind = "build"`** | **`kind = "test"`** |
|---|---|---|
| Produces | an **artifact** (`dist/`) → declared output, content-addressed, exposed as a **provider** | a **result** → captured to `results.txt`, **always exit 0** so a test *failure* is recorded data, not a lost action (the cargo test-run trick) |
| A cache hit means | reuse the artifact | "this test already passed on these exact inputs — skip it" (the `affected` payoff) |
| Consumed axes (§13.6) | `opt_level`→minification, `debug_info`→source maps | `coverage`→test coverage |
| Sandbox default | sealed | sealed (more likely to need permeable) |

The axis split matters: a test action does **not** consume minification, so flipping the
prod build to release does not bust the test cache.

**Determinism caveat for test-result caching:** caching a test result assumes the test is
deterministic; a flaky test cached as "passed" hides a failure. JS test culture is far
leakier than Rust's (tests hit `localhost`, read `process.env`, use `Date.now()`), so this
risk is *higher* for pnpm than cargo. Contract: deterministic by default; flaky tests are
declared non-cacheable. In M1 this is moot — all scripts are non-cacheable by default (§5).

## 4. Routing a generated artifact in (the §14.3 demo)

**Decision: `file:` dependency, name-resolved, wrapper synthesized inline — no `js_package`
rule.** The reasoning (which dissolved an earlier `js_package` proposal):

- The `data` edge is **the same in every case** — a file is content-addressed and
  materialized into the sandbox. The question is only *how the TS source addresses it*.
- **Path-reference** (`import '../gen/config.json'`) needs nothing but the **per-edge
  destination** we already adopted for `data` — bytes land where the import looks. But it
  exercises nothing pnpm-specific; it's the cargo `include_str!` demo in another language.
- **Name-reference** (`import config from '@gen/config'`) is the §14.1 *generated native
  package* differentiator: it routes through pnpm's own resolver (`file:` dep → `package.json`
  in the target dir → `node_modules/@gen/config` symlink). This is the M1 demo.
- Name-reference needs a generated `package.json` *inside the wrapper dir*. That file is
  **never read by Anneal's analysis** — only pnpm reads it at execution — so generating it
  does **not** violate the §14.6 phase wall. (Contrast: `pnpm-workspace.yaml`, which the rule
  *does* parse, must stay static.)
- That wrapper does **not** need a separate rule. The package **name** already lives
  statically in the consumer's own `package.json` (`"@gen/config": "file:…"`, which Anneal
  parses anyway), and the **location** is the per-edge destination. Both have static homes,
  so `pnpm_workspace` synthesizes the one-line wrapper manifest inline. A per-edge
  *destination/name* is **consumer-side mounting metadata**, not the §5.4-forbidden per-edge
  *configuration* — it changes only the consumer's materialization, never the producer's
  output or cache key.

`js_package` would earn its keep only when **one** generated package is consumed by **many**
workspaces (DRY the wrapper; make it a first-class `why`/`affected` target). For the
single-consumer M1 demo it is pure ceremony — promote to a rule later if multi-consumer reuse
appears; the wrapper logic already lives in one place.

The demo proves **routing + composing caches across the boundary**: editing the `.ncl`
regenerates the JSON and rebuilds the consumer; editing only the consumer leaves the Nickel
generation cached. It does **not** prove type safety (§2.3) — Nickel emits data, not types.

## 5. Cacheability — derived and enforced, never claimed

The core correctness stance, expanded in `docs/rules.md` §4. **Hermeticity is not
determinism**, and only the former is enforceable by sealing:

- **Sealing** (no network, scrubbed env, declared inputs only) makes the cache **key**
  complete — a changed input is never missed. Strict on Linux; best-effort on macOS.
- **Determinism** (same inputs → same bytes) sealing **cannot** provide. A sealed script can
  still embed a timestamp, random seed, per-build hash, or iteration order into its output.

So the rule is **not** "sealed → cacheable" but:

> **sealed → key trustworthy. sealed *and verified reproducible* → output safe to cache.**

Reproducibility is *falsifiable, not provable* — a double-build byte-compare
(`verify_correctness_neutral`) is a one-sided test: a diff decisively rejects, but agreement is
only evidence, and rare scheduling-dependent races (thread-completion order, `readdir` order)
can evade any finite sample (`docs/rules.md` §4). A user therefore never declares "cacheable" —
they declare a *constraint* (`sealed`) the system enforces, and the system *earns* the cache by
accumulating reproducibility evidence (N-build sampling by default) or, where the race risk
warrants it, by removing the variance at the source (`SOURCE_DATE_EPOCH`, fixed seeds, or a
deterministic-execution sandbox). That gate runs off the hot path, not in the executor.

### M1 default: non-cacheable + snapshot-accelerated

Because reproducibility is unproven until measured, **every user script is non-cacheable by
default — and that is nearly free**, because the inner tool's incremental state is restored
via snapshot:

- **What "non-cacheable" means:** no action-cache lookup; the script *always re-runs*. We
  never trust a skip we haven't proven safe.
- **What a script restores (v1):** `install`'s `node_modules` snapshot, **read-only** — so the
  script can see its dependencies. Declared outputs are *captured* into the CAS so providers
  work (re-produced each run); the source is materialized as inputs.
- **A script's *own* build-incremental snapshot is deferred.** The inner tool's accelerator
  dirs (`.tsbuildinfo`, `node_modules/.vite/`) would be a **second** snapshot the script
  *saves* under a *different* key (source/toolchain-coarse, unlike `node_modules`'
  lockfile-coarse key). An action carries one `snapshot_key` today, so v1 restores `install`'s
  snapshot and re-runs the tool without warming its own incremental cache; multi-snapshot
  support is a follow-up. (Any such build snapshot is governed by §1.4 and the neutrality
  harness; a non-output-neutral incremental mode is disabled rather than snapshotted.)
- **The cost we accept:** a non-cacheable producer re-runs every time, so a *non-deterministic*
  one churns its output digest and forces downstream misses. A deterministic one keeps a
  stable digest and downstream still hits. We do not *promise* the latter — that is the point
  of non-cacheable.
- **Kernel policy:** script actions use `CachePolicy::SnapshotAccelerated` (`docs/rules.md`
  §5) — restore a snapshot to run, never action-cache. They share `install`'s `snapshot_key`
  to restore `node_modules` (read-only; they never save it), which is the concrete form of
  "the edge carries the install-snapshot identity" (§6). `install` itself stays
  `SnapshotBased` (cacheable; it owns and saves the snapshot).
- **What a real cache hit would add:** eliminating engine startup entirely (valuable for slow
  tsc/vitest starts, with no persistent worker in M1 — §10). That is the **deferred,
  documented opt-in**: passing the reproducibility gate promotes a script to (effectively)
  `SnapshotBased`. **There is no `cacheable` attribute** — graduation is a *system* action
  after verification, never a consumer assertion (`docs/rules.md` §4). A consumer's only
  cacheability-relevant lever is marking a script `permeable` (needs network) — which moves it
  *toward* non-cacheable, never toward an unsafe cache. So a BUILD author has no foot-gun that
  could poison the cache.

This is deliberately conservative: install caching (the slow part, the §15.1 CI wedge) is
the safe high-value win; user scripts are correct-and-fast by default; the only thing
deferred is an optimization that must pass a test to turn on.

## 6. `node_modules`, the install snapshot, and the install→script edge

`node_modules` is the install layer's `target/` — and the reasoning for why it is a
**snapshot, not a content-addressed Output**, is the canonical worked example of the
distinction in `docs/rules.md` §5.

**It is necessary data, but *re-derivable* data.** A script genuinely cannot run without
`node_modules` (no `tsc` without it), so it feels load-bearing for identity. It is not: its
content is a pure function of the **lockfile** (+ toolchain + platform), so the *identity* is
the lockfile digest and `node_modules` is merely its expensive materialization — exactly the
`rustc`-version-vs-`rustc`-binary relationship. Delete it and the system re-derives via `pnpm
install --frozen-lockfile`; output is unchanged, only slower. So it sits on the *availability*
side, delivered by snapshot, not in any consumer's cache key.

**Snapshot key:** `(platform, pnpm major, pnpm-lock.yaml digest)`. Platform is **mandatory** —
pnpm installs only the `os`/`cpu`/`libc`-matching variant of platform-specific
`optionalDependencies` (e.g. `@esbuild/darwin-arm64` vs `@esbuild/linux-x64`), so `node_modules`
content legitimately differs per platform. **Note what is *absent*: the Node version.** See the
decision below for why.

**Determinism (researched):** pnpm is strongly deterministic here — `pnpm-lock.yaml` is fully
resolved and integrity-pinned (exact versions + content hashes), and `node_modules` is
hardlinks from a content-addressed store plus *relative* symlinks. The one historical
non-determinism source is dependency lifecycle/`postinstall` scripts (native `node-gyp` builds:
bcrypt, sqlite3, …), which compile ABI-tied `.node` binaries against the running Node version —
the *only* channel by which Node version affects `node_modules` content.

**Decision — pnpm ≥ 10.0.0, no lifecycle scripts at install.** Rather than guard against that
channel by keying on Node version, we close it:

- **Minimum pnpm 10.0.0**, where dependency lifecycle scripts are **blocked by default**. This
  gives a single behavioral baseline and the clean substrate to *enforce* the policy (vs.
  fighting a default-on `--ignore-scripts`).
- **Lifecycle scripts are not run at install** (we do not honor `onlyBuiltDependencies`). Install
  is pure resolution + extraction. A native build that genuinely needs a compile step is declared
  as an explicit `kind = "build"` action — sealed, declared inputs/outputs — which is *more*
  correct than an opaque install hook (it converts arbitrary install-time code into a first-class,
  modelable action; "wrap, don't decompose").

This is architecturally consistent, not merely convenient: lifecycle scripts are the antithesis
of hermeticity, and pnpm 10 blocking them by default is the ecosystem conceding the same point.

**Consequence — Node version drops from the `node_modules` key.** With no install-time
compilation, the only native artifacts present are **prebuilt, ABI-stable** ones: N-API or
standalone binaries shipped *in* platform `optionalDependencies` (esbuild, swc, `@rollup/rollup-*`,
lightningcss, `@next/swc`), which pnpm merely extracts. N-API is ABI-stable across Node versions
by design, so these are **platform-keyed but Node-version-agnostic** — already covered by
`platform`. Old-style NAN `node-gyp` addons that need a compile or a `prebuild-install`/`node-pre-gyp`
download (both lifecycle scripts) simply do not function and are **not supported** (declare a build
action instead). So `node_modules` content is a pure function of `(platform, pnpm major, lockfile)`.

**Node version does not vanish from the system** — it remains part of a *script action's*
toolchain identity (Node runs the test/build script and can change its *result*: V8/language
behavior, test outcomes) and of any build-script snapshot (`.tsbuildinfo`). The decision is
narrow and exact: Node is irrelevant to *what `node_modules` contains*, relevant to *what running
a script produces*.

Other consequences, all acceptable for a build sandbox: `husky` git-hook setup (a `prepare`
script) won't run — a dev-machine concern, not a build step; `patch-package`-via-`postinstall` is
replaced by pnpm's native **`patchedDependencies`** (lockfile-pinned, deterministic, already
covered by the lockfile digest). The §14.3 demo and typical TS stacks (tsc, vitest/esbuild) are
pure-JS or N-API and **unaffected**.

**The key insight: the snapshot is safe *regardless* of byte-determinism.** Safety comes from
**re-derivation**, not snapshot fidelity: on a cold or distrusted snapshot, `--frozen-lockfile`
rebuilds against the integrity-checked lockfile, so a stale snapshot can never produce a *wrong*
result — only a slower build. Determinism therefore governs only **hit-rate and cross-machine
sharing**, never correctness. (Contrast a content-addressed Output, where non-determinism is
fatal — digest churn means it never hits.) This is precisely why `node_modules` is a *good*
snapshot and would be a *bad* Output.

**The key insight: the snapshot is safe *regardless* of byte-determinism.** Safety comes from
**re-derivation**, not snapshot fidelity: on a cold or distrusted snapshot, `--frozen-lockfile`
rebuilds against the integrity-checked lockfile, so a stale snapshot can never produce a *wrong*
result — only a slower build. Determinism therefore governs only **hit-rate and cross-machine
sharing**, never correctness. (Contrast a content-addressed Output, where non-determinism is
fatal — digest churn means it never hits.) This is precisely why `node_modules` is a *good*
snapshot and would be a *bad* Output.

**The install→script dependency.** Unlike `cargo_workspace`, where the build action and
test-compile actions are mere snapshot-sharing *siblings* (each runs a self-sufficient `cargo`
subcommand that re-resolves from a cold `target/`), pnpm scripts are **true prerequisites** of
install: `pnpm run <x>` does *not* bootstrap dependencies, so it fails without a populated
`node_modules`. Every script action therefore depends on install — uniformly, even a zero-dep
script (the cost of a spurious ordering edge is nil; a missing one is a broken build). The edge
carries the install snapshot's **identity** (its key/digest) — enough for ordering and to put
the install state in each script's cache key — while the gigabytes of `node_modules` arrive via
`restore`, not as a materialized Output. The routing chain is therefore:

```
nickel_eval  →  install  →  { test, build, … }
```

— the generated `@gen/config` feeds *install* (the `file:` dep is wired during install), not the
scripts directly. For the install snapshot to be self-contained, the routed package is **injected**
(copied into the virtual store) rather than symlinked out to `.mybuild/gen/`, so the snapshot
carries it and a script's sandbox needs nothing materialized beyond the restore. (Injection +
the directory-member-set-known-at-execution shape intersect the deferred **tree-artifact** work.)

## 7. Axis interpretation (§13.6)

| Axis | `pnpm_workspace` mapping |
|------|--------------------------|
| `opt_level` | minification (e.g. `NODE_ENV=production`, bundler minify) |
| `lto` | ignored |
| `debug_info` | source maps |
| `sanitizer` | ignored |
| `coverage` | test coverage (consumed by `kind = "test"` actions only) |

A script action consumes only the axes relevant to its kind, so unrelated config changes
don't bust its cache key.

## 8. Milestone-1 scope vs. deferred

**In scope (M1):**

- `install` action: resolve + install (pnpm ≥ 10, **no lifecycle scripts**), cached + snapshot
  (`node_modules`, store) keyed `(platform, pnpm major, lockfile digest)` — §6.
- Static introspection of `pnpm-workspace.yaml` + `package.json`(s) for members/scripts.
- Script *discovery*; user declares `scripts = { name: { kind, outputs? } }` with explicit
  `kind`.
- All script actions **non-cacheable + snapshot-accelerated**, sealed by default.
- `data` routing as a `file:` dependency, name-resolved, wrapper synthesized inline.
- The §14.3 Nickel → TS demo with composing caches.
- Axis mapping per §7; toolchain (`node`, `pnpm`) discovered on PATH (ad-hoc, like cargo).

**Deferred:**

- **`sealed` + reproducibility-gated → cacheable** opt-in for user scripts (§5). The
  documented extension point; not built in M1.
- **External-dependency vendoring** (registry deps; offline store population).
- **Separately-addressable script targets** (`//app:test`) — falls out of named output
  groups + demand-driven pruning, the *same* deferral `cargo_workspace` shares.
- **`workspace:` member routing** for a generated package — would force the §14.6 staged
  materialize pass; `file:` sidesteps it.
- **`js_package` rule** — only if multi-consumer reuse of a generated package appears (§4).
- Structured test-result parsing for JS runners (TAP/JSON) — basic exit-based pass/fail
  first, mirroring how `cargo_workspace` got structured results after the basics.
- `register_toolchain` for `node`/`pnpm` (the WORKSPACE toolchain item, shared with cargo).

## 9. Decisions locked in this design pass

1. Stance **(ii)**: install inferred + auto-owned; build/test scripts declared.
2. `kind` is **explicit** — no name-based convention.
3. Routing is a **`file:` dependency**, **name-resolved**, wrapper **synthesized inline** in
   `pnpm_workspace` — **no `js_package`** for M1.
4. Cacheability is a **derived, reproducibility-verified property of a sealed action**, never
   a user claim. Default **non-cacheable + snapshot**; sealed-cacheable opt-in **deferred**.
5. **pnpm ≥ 10.0.0 required**, and **lifecycle scripts are not run at install** (no
   `onlyBuiltDependencies`). Native builds that need a compile step are declared as explicit
   `kind = "build"` actions, not opaque install hooks. Supported native modules are
   N-API/prebuilt via platform `optionalDependencies`; `node-gyp`-at-install is unsupported.
   Package patching uses pnpm's native `patchedDependencies`, not `patch-package`.
6. **`node_modules` snapshot key = `(platform, pnpm major, pnpm-lock.yaml digest)`** — Node
   version is *dropped here* (closed by decision 5), but **retained as a script action's
   toolchain identity**, since Node affects what a running script *produces*, not what
   `node_modules` *contains*.
