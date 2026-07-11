# Anneal

Anneal is a pre-1.0 build system for polyglot repositories. It wraps native tools such
as Cargo, pnpm, and Nickel, then puts content-addressed caching, declared inputs, and
sandboxed execution around them.

Anneal is under active development. The repository contains both working software and
designs for later product stages; this README is the authoritative summary of what the
current tree implements. Update the matrix when a feature's availability changes.

Status labels:

- **Available** — implemented, exposed through the current API or CLI, and covered by tests.
- **Partial** — useful today, with an important limitation called out below.
- **Experimental** — infrastructure or a vertical slice exists, but it is not yet a
  complete production path.
- **Planned** — designed or tracked, but not implemented.

## Current implementation status

| Area | Status | Current behavior and remaining work |
|---|---|---|
| Loading and analysis | **Available** | Loads the requested target's transitive package closure; whole-workspace loading supports reverse-dependency queries. Generated-path collisions and generated-output/source shadowing fail during analysis. Enforcing one sweeping workspace owner per package remains planned. |
| Local execution and caching | **Available** | Executes independent actions concurrently, caches content-addressed results, reuses warm tool state, streams completion, preserves independent work after failures, skips failed dependents, and reports bounded failure output. |
| Linux sandbox | **Available** | Bubblewrap provides namespace-based filesystem and network isolation. Linux sandbox tests run through the Docker CI path. |
| macOS sandbox | **Partial** | Seatbelt denies undeclared file access and network access according to action policy, with scrubbed environments and declared toolchain roots. It is graded `LoudBestEffort`, not Linux-equivalent `Enforced`; a Linux VM execution path is planned after Milestone 1. |
| Focus-cone execution | **Experimental** | `build` and `test` derive dirty targets and their dependents from `git status`, run that cone as `Incremental`, and color the rest `Hermetic`. The monotonicity invariant is enforced. There is no hysteresis or pinning yet; committing makes the tree clean and can trigger a Hermetic rebuild because Incremental and Hermetic action contracts have different keys. |
| Trust, tiers, and provenance | **Partial** | Enforcement grades, local/promotable cache tiers, cache-entry provenance, and `--require-enforced` are implemented. There is no remote/shared cache or `--explain-trust` CLI yet, so `Promotable` currently records eligibility rather than uploading anything. |
| Persistent state model | **Partial** | Typed phase-separated and attested interleaved state lower to the working snapshot engine; Hermetic actions cannot mutate interleaved state. The action model currently supports one state tree per action, does not enforce a single phase-separated producer structurally, and conservatively caps every snapshot owner at the local tier. |
| Analysis-time tool queries | **Experimental** | `QuerySpec` provides sealed, network-denied queries with stable roots, captured stdout, and CAS-backed caching; `RuleContext` and the CLI pipeline are wired for it. No production rule uses it yet: `cargo_workspace` still hand-parses workspace structure. Queries cannot yet read phase-separated state or consume generated artifacts. |
| `cargo_workspace` | **Partial** | Builds coarse Cargo workspaces, maintains warm `target/` state, splits library unit-test compilation from execution, supports doc/integration tests, fixed-output crates.io acquisition from a committed lockfile, declared native libraries, and Rust flag axes. Missing pieces include authoritative `cargo metadata` staging, binary/bin-unit targets, per-integration-binary targets, separately addressable tests, generated lockfiles, and non-crates.io acquisition. |
| `pnpm_workspace` | **Partial** | Performs frozen offline installs into phase-separated state and runs explicitly declared test/build scripts. External package acquisition, lifecycle/native-build actions, script cache promotion, structured JS test results, and a portable content-addressed pnpm store remain planned. |
| `nickel_eval` | **Partial** | Exports one self-contained Nickel source to a selected supported format and exposes it to downstream rules. Multi-file Nickel imports are not yet declared or supported. |
| Generic and routing rules | **Available** | `filegroup`, `alias`, and `genrule` work across package boundaries. Generated data consumed by actions can be routed into their sandbox paths. Alias targets forward providers but intentionally do not re-home materialization routes. |
| Worktree materialization | **Available** | `anneal materialize` mirrors generated inputs consumed by a target into tree-shaped paths, tracks ownership/digests, avoids mtime churn, refuses destructive overwrites by default, and supports `--check`, `--list`, `--clean`, and `--force`. It materializes the actual consuming target, not an alias to it. |
| Dependency queries | **Partial** | `affected --since`, `why <from> <to>`, and `why <target> --since` are available. `affected` currently omits untracked-but-unadded files; `why --all`, `affected --explain`, and general `query`/`aquery` commands are planned. |
| Toolchains | **Partial** | First-party rules resolve closure-complete Nix toolchains from `ANNEAL_TOOLCHAIN_MANIFEST`; toolchain identity, roots, action environment, and derived `PATH` enter the action contract. Anneal-managed provisioning and user-facing `WORKSPACE` toolchain registration are planned, so the current adopter path requires Nix. |
| Fixed-output downloads | **Available** | The executor downloads hash-pinned blobs over a Rust TLS stack, retries transient failures, verifies the expected digest before CAS admission, and skips the network when the pinned blob is already present. |
| Remote cache and execution | **Planned** | No remote cache backend, GitHub Actions cache transport, or remote executor exists yet. All current reuse is from the local `.anneal` store. |
| Store lifecycle | **Planned** | CAS, action-cache, snapshot, and warm-state garbage collection/eviction are not implemented; `.anneal` can grow without bound. Cache inspection and cleanup commands are also planned. |
| Diagnostics and input sensing | **Planned** | Structured load/analysis/execution errors exist, but stable error codes, `anneal explain`, curated undeclared-input diagnostics, and `anneal sense`/audit tracing are design work, not current commands. |
| Platforms | **Partial** | Linux and macOS are supported at different enforcement grades. Windows is not supported. Cross/exec-platform transitions remain planned. |

## Current CLI

The current binary exposes:

```text
anneal build <target>
anneal test <target>
anneal affected --since <git-ref>
anneal why <from> <to>
anneal why <target> --since <git-ref>
anneal materialize [<target>] [--check|--list|--clean] [--force]
```

Configuration flags include platform, optimization, LTO, debug-info, sanitizer,
coverage, execution mode, job count, and the minimum enforcement floor. Run
`anneal --help` for the exact grammar.

Commands described elsewhere but absent from this list—including `init`, `run`,
`check`, `exec`, `query`, `aquery`, `cache`, `status`, `sense`, `sync`, and `explain`—are
planned rather than implemented.

## Building and testing

The supported contributor and current adopter environment is Nix:

```console
nix develop
cargo test --workspace
cargo run -p anneal-cli -- --help
```

The flake also exports the CLI and toolchain manifest:

```console
nix build .#anneal
nix build .#toolchain-manifest
```

Some real ecosystem tests are gated by `ANNEAL_NETWORK_TESTS=1`; the macOS CI lane
runs the gated fetch and native-library paths. The heavier transitive crates.io fetch
test remains explicitly ignored and is run manually when needed.

## Documentation map

- [`docs/why-anneal.md`](docs/why-anneal.md) describes the product thesis and target
  experience. Some command transcripts intentionally show planned behavior; use this
  README for current availability.
- [`DESIGN.md`](DESIGN.md) is the living architectural decision document. It contains
  both landed mechanisms and explicitly named deferrals.
- [`TODO.md`](TODO.md) is the detailed implementation backlog and historical engineering
  record. It is more granular than this matrix.
- [`docs/rules.md`](docs/rules.md) documents the rule/engine contract and trust boundary.
- [`docs/sandboxing.md`](docs/sandboxing.md) and
  [`docs/sandbox-contract.md`](docs/sandbox-contract.md) document platform guarantees
  and the rule-author-facing sandbox contract.
- [`build-system-design.md`](build-system-design.md) and
  [`MILESTONE-1-PLAN.md`](MILESTONE-1-PLAN.md) retain the broader design and milestone
  history; they are not implementation-status references.

