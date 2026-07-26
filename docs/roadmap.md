# Anneal roadmap

> **Status:** Directional product sequencing, not a release commitment.
> The [README feature matrix](../README.md#current-implementation-status) is the authoritative
> account of what works today.

Anneal's roadmap is organized by confidence boundary rather than version number. A capability
moves forward when its prerequisites and correctness tests exist, not merely when its design is
written down.

## Now — the current product

Anneal currently provides a local build-system vertical slice:

- an explicit target and action graph across Cargo, pnpm, Nickel, and generic rules;
- generated-file routing between native-tool boundaries;
- explicit worktree materialization for generated inputs needed by IDEs or unwrapped tools;
- sandboxed actions with Linux `Enforced` and macOS `LoudBestEffort` grades;
- a local content-addressed store and exact action-result cache;
- managed warm state for selected native tools;
- focus-cone execution based on local Git status;
- dependency-impact and shortest-path queries through `affected` and `why`;
- Nix-manifest-backed first-party toolchains and pinned fixed-output downloads.

The current product is pre-1.0. It requires Nix for the supported adopter path, has no remote
cache or store garbage collection, executes the full analyzed action set for ordinary builds,
and has correctness-hardening work listed in [`TODO.md`](../TODO.md).

## Next — production confidence

The next stage is about making the existing promise boring and dependable, not expanding the
surface area.

### Cache and state correctness

- Complete action identity, including declared output mappings.
- File-digest memoization robust to same-size/same-mtime replacement.
- Explicit generic-action cacheability policy.
- Collision-resistant persistent-state owner identity.
- Broader warm-versus-cold neutrality verification.

### Operational completeness

- Store inspection, retention, and garbage collection.
- Demand-driven action pruning.
- Process-level interruption and concurrency tests.
- Stable, actionable diagnostics.
- Complete subprocess coverage for generated-file materialization.

### Adoption

- A toolchain story that does not require adopters to understand the internal Nix manifest.
- Honest onboarding based on configuration Anneal can produce and execute today.
- Better current-rule reference documentation and examples.

### Performance contract

Anneal should:

- win decisively when an exact cache hit or unchanged generated output avoids native work;
- impose bounded, measured overhead when it must invoke the same native tool;
- preserve correctness when warm state is discarded;
- report performance by realistic scenario instead of requiring every incremental miss to beat
  an unwrapped native invocation.

## Later — scale and richer adoption

These capabilities are plausible extensions, but are not required to validate the current
“wrap, don't replace” thesis.

### Shared cache

A remote/shared cache follows only after action identity, local cache correctness, provenance,
admission rules, retention, and poisoning tests are production-ready. The initial goal is
shared exact outputs and portable ecosystem stores, not transportation of native working
directories.

See [`proposals/remote-cache.md`](proposals/remote-cache.md).

### Input sensing

Input sensing may make declarations and sandbox failures easier to author and diagnose.
Observation proposes dependency declarations; it never silently expands the execution-time
input boundary.

See [`proposals/input-sensing.md`](proposals/input-sensing.md).

### Stronger macOS isolation

A Linux VM could provide Linux-equivalent enforcement on macOS after the local Linux executor,
CAS transport, lifecycle, and performance model justify the operational cost.

See [`proposals/linux-vm.md`](proposals/linux-vm.md).

### Additional graph capabilities

- Staged analysis for generated manifests that shape the target graph.
- Dynamic tree artifacts.
- Portable Cargo and pnpm ecosystem stores.
- Additional first-party rules, including the
  [Simplex proposal](simplex-rules.md).
- A stable third-party rule API after first-party rule contracts stop changing rapidly.

## Deliberately outside the validation gate

Anneal does not need remote execution, a resident daemon, persistent workers, or full
Bazel-compatible rule extensibility to prove that native tools can be wrapped behind a useful
hermetic graph. Those may be revisited if measured workloads require them.
