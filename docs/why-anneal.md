# Anneal — trustworthy builds for polyglot monorepos

**Anneal is a build system that wraps the tools you already use — cargo, pnpm, go — instead of replacing them, and makes their caching actually trustworthy by enforcing, not just declaring, what every build step is allowed to read.**

> **Status:** Anneal is in active development (pre-1.0). This document describes the system as designed and being built; the [design document](../DESIGN.md) is the engineering-level companion. Where a feature is roadmap rather than current, this doc says so.

This page is for you if you work in a monorepo at a small-to-mid-sized team and some of the following sounds familiar.

---

## The problems you probably have

**"Works on my machine."** A build passes locally and fails in CI, or vice versa, and the diff session that follows finds the cause in something nobody declared anywhere: a different toolchain version, an `.env` file, a globally installed binary, a `~/.gitconfig` setting. The build's real input set and its written-down input set drifted apart, silently, months ago.

**You stopped trusting your build cache.** Maybe it's Turborepo, Nx, or a homegrown hash-the-inputs script. It works most of the time — until the day it serves a stale artifact, someone loses an afternoon to a bug that didn't exist, and the team learns the ritual: when in doubt, `--force`, `rm -rf node_modules`, clean build. Every forced rebuild is the cache admitting it can't be trusted; every clean build is paying for that admission in minutes, forever.

**CI rebuilds the world, or lies about not needing to.** Either every PR builds and tests everything (slow, expensive), or you've got path-glob filters deciding what to skip (`paths: ["backend/**"]`) — which is a hand-maintained, slowly-rotting approximation of your real dependency graph. The day a filter misses a real dependency, you ship a break that CI said was green.

**Cross-language seams are held together with tape.** A schema or config feeds codegen into two languages, and the generated files drift from their source. So you add a CI tripwire — regenerate and diff — which fails on formatting noise, gets a `--no-verify` culture, and still doesn't tell you *what* to rebuild when the schema changes.

**You looked at Bazel and closed the tab.** Correct instinct, honestly: Bazel-class tools solve all of the above, but the price is converting everything — BUILD files for every package, abandoning cargo/pnpm workflows your team knows, and a permanent maintenance treadmill keeping rules in sync with upstream tools. For a 15-person team that price never amortizes.

The gap between "fast to adopt but can't be trusted" and "trustworthy but costs a quarter to adopt" is where Anneal lives.

---

## The core idea: enforcement, not declaration

Every caching build tool asks the same question: *what are this step's inputs?* The lightweight tools take your word for it — they hash what you declared and hope you declared everything. The heavyweight tools make you declare everything exhaustively, which is why adopting them is so expensive.

Anneal's answer is different:

> You don't need fine-grained input *declaration* to get trustworthy caching — you need fine-grained input *enforcement*. Anneal runs every build step in a sandbox where undeclared inputs aren't just unhashed, they're **unreadable**. A step that succeeds has *proven* its input set is complete, because nothing else existed for it to read.

This one move changes the economics of correctness:

- **Coarse declarations are fine.** Declaring "this whole cargo workspace" as one unit costs you some cache granularity — never cache *correctness*. A coarse cache key rebuilds slightly too often; it never serves you a stale artifact. You can adopt Anneal with three lines of config and refine granularity later, only where profiling says it's worth it.
- **Drift is impossible, not unlikely.** When someone adds a dependency on a new file without declaring it, the build fails immediately with a message naming the file — instead of poisoning the cache for weeks.
- **Toolchains are inputs too.** Anneal pins and provisions the compiler itself (your `rust-toolchain.toml` version, fetched and verified). "It compiles differently on my machine" stops being a possible sentence.

And the part that makes Anneal different from previous attempts at this: **it wraps your native tools instead of replacing them.** Cargo still builds your Rust; pnpm still manages node_modules; their lockfiles remain the source of truth. Anneal manages the *boundaries* — what each tool may read, where its outputs go, when its work can be reused — including the hard part: each tool's own incremental state (cargo's `target/`, Go's build cache) is kept warm and managed rather than destroyed, so the inner loop stays fast. You keep your tools' speed and your team's muscle memory.

---

## What it looks like

### The first ten minutes

Anneal is a single static binary. Setup scans your repo and scaffolds config without touching any existing file:

```console
$ anneal init --detect
  detected: cargo workspace — 9 members, rust 1.87.0 (rust-toolchain.toml)
  detected: pnpm workspace — 4 packages (pnpm-lock.yaml)
  wrote WORKSPACE
  wrote BUILD
  no existing files were modified — anneal owns .anneal/ and nothing else
```

The generated `BUILD` file is small and readable — rule invocations, not a programming exercise:

```python
# BUILD
cargo_workspace(
    name = "backend",
    manifest = "Cargo.toml",
)

pnpm_workspace(
    name = "web",
)
```

The first build is cold, and Anneal says so rather than letting you wonder:

```console
$ anneal build //:backend
  provisioning rust 1.87.0 (pinned, digest-verified) … done
  CargoFetch //:backend    network: crates.io — every byte pinned by Cargo.lock
  CargoBuild //:backend    cold: 2m41s
  note: first build provisions and proves; warm builds are the product

$ # edit one file, build again
$ anneal build //:backend
  CargoBuild //:backend    warm: 1.9s
```

That second number is the point of the whole architecture: cargo's incremental state is alive and managed inside the sandbox, so the inner loop is cargo-fast — not container-cold.

### Your first sandbox violation (this is a feature)

At some point in the first hour, Anneal will fail a build that "worked before." This is the product working:

```console
$ anneal build //:backend
error[AN0231]: undeclared input
  CargoBuild //:backend read `.env`, which is not in its declared sources.

  Why this failed: `.env` exists on your machine but not in CI or on your
  teammates' machines. Without this error, this build could silently differ
  across machines — that is exactly the bug Anneal exists to catch.

  To declare it:    srcs = glob(["**/*.rs", "Cargo.*"]) + [".env"]
  Or (recommended): stop reading .env during builds; inject config at runtime.

  More detail: anneal explain AN0231
```

Every violation names the file, the build step, and the one-line fix — and tells you why it's a catch rather than an obstacle. The errors you'll actually hit (`.env` files, toolchain overrides, network access from `build.rs`, postinstall scripts) each have curated diagnostics.

### CI and the shared cache

One CI block. CI builds in Anneal's fully hermetic mode and populates a shared cache:

```yaml
# ci.yml
- run: anneal build //... --require-enforced
- run: anneal test $(anneal affected origin/main..HEAD)
```

Two things are happening there:

- `--require-enforced` makes CI fail loudly rather than ever run with weakened sandboxing — the shared cache only ever contains results built under full enforcement.
- `anneal affected` computes which targets a change actually touches **from the enforced dependency graph**, not from hand-written path filters. It's the path-glob CI filter you have today, except derived instead of maintained, and incapable of the silent-miss failure mode.

Then your colleague pulls the branch:

```console
$ anneal build //:web
  PnpmInstall //:web   cache hit (built in CI, enforced)
  ViteBuild   //:web   cache hit (built in CI, enforced)
  0 actions executed, 14 cache hits — 0.8s
```

Cache hits are safe to trust *because* of the enforcement story: every entry was produced by a step that provably read only its declared inputs, and every entry carries provenance — what built it, where, under what sandbox strength. You can interrogate any of it:

```console
$ anneal query --explain-trust //:backend
  CargoBuild //:backend
    sandbox: linux namespaces (enforced)
    cache: local tier — mutates managed cargo target/ state
      attested: "cargo fingerprint reuse is sound under --locked +
                 pinned toolchain"; revocable by epoch
    hermetic variant: promotable — this is what CI builds and shares
```

That last distinction — your warm dev builds stay on your machine; only cold, deterministic, fully-enforced builds enter the shared cache — is automatic. You never think about it; the tier system arbitrates which results are trusted where.

### The cross-language seam

This is where wrapped tools usually fall apart and where a real graph pays off. Say a schema feeds generated code in two languages:

```python
nickel_eval(
    name = "api_schema",
    entry = "config/api.ncl",
)

genrule(
    name = "api_types_ts",
    srcs = [":api_schema"],
    tool = "//tools:codegen",
    outs = ["web/src/generated/api.ts"],
)
```

Now `anneal build` regenerates exactly what a schema change actually affects — and, because the schema export is byte-deterministic, a change that doesn't alter the output (a comment, a reformat) produces identical bytes and **the rebuild stops right there**. Nothing downstream re-runs. This is what retires the regenerate-and-diff CI tripwire: generated code can't drift, because it's a build product with a real edge to its source.

For files your IDE and unwrapped tools need to see on disk, `anneal sync` materializes generated outputs into the worktree and tracks their staleness — so partial adoption doesn't mean half your tooling goes blind.

### Adopting one package at a time

You don't convert the repo. You wrap one package and tell Anneal the rest exists:

```python
cargo_workspace(
    name = "backend",
    manifest = "Cargo.toml",
    unmanaged_deps = ["../legacy"],   # not converted, just declared
)
```

Unmanaged dependencies are hashed coarsely — the whole tree — which is *conservative*: it can over-rebuild, never under-rebuild. When the coarseness costs you cache hits, Anneal can tell you where:

```console
$ anneal query --coarse-edges
  //:backend ← ../legacy  (opaque tree, 412 files)
    every file change invalidates; refine when this hurts
```

Refinement is a performance knob you turn when profiling says so — never a correctness chore you must finish before the system works. A half-converted repo is coarse-but-correct, not fine-but-wrong.

---

## How it compares

| | Turborepo / Nx / scripts | **Anneal** | Bazel / Buck2 |
|---|---|---|---|
| Adoption cost | Minutes | **An afternoon** | Months |
| Keeps native tools (cargo, pnpm)? | Yes | **Yes** | No — replaced by rules |
| Caching correctness | Declared hashes; stale-cache incidents are a known tax | **Enforced; undeclared inputs are unreadable** | Enforced |
| Toolchain pinning | No | **Yes, provisioned + verified** | Yes |
| Affected-target CI | Path globs (hand-maintained) | **Derived from the enforced graph** | Derived |
| Warm inner loop | Native-fast | **Native-fast (managed tool state)** | Tool-replaced; depends |
| Cross-language graph | No | **Yes** | Yes |

The middle column is the historically hard one — projects that tried it (Earthly, Please, Pants) mostly died on one of two rocks: wrapping at container granularity (inner loop too slow to live with) or quietly requiring full conversion anyway. Anneal's bets against those two failure modes are, respectively, *managed tool state* (your tool's own incremental cache, kept warm inside the hermetic boundary) and *enforcement-makes-coarse-safe* (partial adoption that's structurally sound, not aspirationally sound). The [design doc](../DESIGN.md) goes deep on both.

**A note on AI agents**, because it's probably part of your present or near future: agents amplify every problem on this page. They don't read your README's "remember to rm -rf node_modules" folklore, they make undeclared-dependency mistakes at machine speed, and they'll happily trust a stale cache. A build system where every step is sandboxed, every input enforced, and the graph is queryable (`anneal query` answers "what depends on this?" precisely) is the substrate that makes agent-driven development in a monorepo something other than terrifying.

---

## What Anneal asks of you — and what it never asks

**Asks:**

- A `WORKSPACE` file and a small `BUILD` file per managed package (often generated by `init --detect`).
- Lockfiles and pinned toolchains — `Cargo.lock` committed, a `rust-toolchain.toml` or equivalent. (You likely have these already; Anneal makes them load-bearing.)
- Tolerance for the first afternoon's sandbox violations, which are real findings about your build, surfaced all at once instead of one incident at a time.

**Never asks:**

- Rewriting dependency graphs into BUILD files — your lockfiles remain the source of truth.
- Replacing cargo/pnpm/go or abandoning their CLIs for local development.
- Converting the whole repo, ever. The unmanaged-dependency escape hatch is a permanent, supported state, not a migration grace period.
- A daemon. Anneal is daemonless by design — all state lives on disk in content-addressed stores, `--watch` exists as an opt-in foreground accelerator, and "restart the daemon to fix it" is not a sentence in this ecosystem.

---

## Honest limits

In the spirit of a tool whose entire premise is not lying to you:

- **The first build is slower than native.** Provisioning, ingestion, and a cold build. The tenth build is the comparison that matters, and the warm numbers are the project's primary benchmark gate.
- **macOS sandboxing is graded, not absolute.** On Linux, enforcement is kernel-level: undeclared files don't exist inside the sandbox. On macOS, Anneal uses Seatbelt policies — violations fail loudly, but it's a strong mitigation rather than a structural proof, and Anneal treats it that way: Mac-built results stay on your machine and never enter the team's shared cache (Macs still *consume* the shared cache freely). A managed Linux VM that restores full enforcement on Macs — with near-native performance, because Anneal's architecture keeps the VM off the hot path — is on the roadmap.
- **Ecosystem difficulty varies, and Anneal says so per ecosystem.** Go is nearly free. pnpm is easy. Cargo is well-supported but genuinely hard under the hood (it's the flagship). Gradle is adversarial to this whole model — wrappable, but coarsely and with heavy auditing. The docs grade each honestly rather than implying all wraps are equal.
- **Test-result caching is roadmap, not current.** It's the natural next link in the chain (trustworthy caching is what makes cached test results believable at all), and it's designed after the core earns trust.
- **Pre-1.0.** The Milestone 1 target is the thesis demo: a real cargo workspace, hermetically managed, with inner-loop times within shouting distance of bare cargo. If you'd rather watch that land before betting a repo on it, that's the correct read of this document.

---

## Is Anneal for you?

Good fit:

- 5–50 engineers in a monorepo with two or more languages or one correctness-sensitive seam (codegen, native deps, reproducibility requirements).
- You've been burned by a stale cache, or you're paying the clean-build tax to avoid being burned.
- Your CI time grows with repo size instead of change size, or your path-filter config has become load-bearing folklore.
- You evaluated Bazel seriously and correctly concluded the conversion doesn't amortize at your size.

Poor fit (today):

- Single-language repo where the native tool alone is serving you fine — you don't have the problem yet.
- JVM/Gradle-centric builds as the main event.
- You need Windows, or remote build execution, in v1.

If the good-fit list reads like your week, the [design document](../DESIGN.md) explains how each promise above is kept — including the trust model that decides exactly when a cached result may be reused, and what Anneal does to *detect* the failure modes it can't structurally prevent.
