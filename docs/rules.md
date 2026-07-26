# Rule and engine contract

> **Status:** Normative current contract, last reconciled July 14, 2026.
> The [README](../README.md) owns rule availability. Known implementation deviations are
> explicit below and are Priority 0 defects in [`TODO.md`](../TODO.md), not alternate contracts.

## 1. Mechanical boundary

A rule translates one configured target into analysis data:

```text
analyze(context) -> Analysis { actions, providers }
```

Analysis declares work; it does not perform ordinary build execution. The result has two
directions:

- **Actions** are the work this target contributes to the action graph. Their inputs create
  producer/consumer edges.
- **Providers** are the typed interface exposed to dependent targets. The current public
  provider payload is a `FileSet` of symbolic artifacts.

`QuerySpec` is a deliberately narrow exception: analysis may issue a sealed, network-denied,
content-addressed native-tool query through `RuleContext`. No production rule currently uses
that path, and queries cannot consume ordinary generated action artifacts.

## 2. Phase boundary

Rules may inspect source/static declarations and the result of permitted analysis queries.
They cannot inspect an ordinary generated output during the same analysis pass.

Consequences:

- A generated file may be routed directly into a downstream execution action.
- `anneal materialize` may export routed generated inputs for a native tool or IDE.
- A generated manifest that must reshape Anneal's own target graph requires an explicit
  materialize-and-rerun workflow today.
- Automatically suspending analysis on generated content requires a future staged-graph design.

## 3. Artifacts, providers, and input roles

An artifact carries a logical path and one of two sources:

- a source digest known during analysis; or
- a named output of a producing action, resolved during execution.

Action inputs have role-specific builders:

| Role | Purpose | Worktree materialization |
|---|---|---|
| `source_input` | Source content already present in the repository | Never mirrored |
| `writable_source_input` | Private mutable copy of declared source content | Never mirrored |
| `dependency_input` | Internal producer output such as a fetched blob or test binary | Never mirrored |
| `data_input` | File routed to a native-tool-relative path | Produced content is eligible for `anneal materialize`; source content is already present |

Rules must use the role matching the artifact's product meaning. `mirror_to_tree` is derived by
`data_input`; it is not a second user-maintained routing list.

Aliases forward providers but intentionally do not create new materialization ownership at the
alias label. Materialize the actual consuming target.

## 4. Action contract

A rule is responsible for declaring every property that can affect observable execution:

- command and arguments;
- working directory;
- declared inputs and their sandbox paths;
- declared outputs and their logical names/paths;
- environment;
- toolchain identities and readable roots;
- execution mode and network capability;
- consumed configuration axes;
- persistent-state use;
- cache policy; and
- fixed-output digest, where applicable.

Action identity must distinguish any two action contracts that may produce or expose different
results. Scheduling-only policy, such as job concurrency, does not enter identity. Enforcement
grade is provenance: it controls whether a result satisfies the caller's trust floor, not what
the action computes.

### Known action-identity deviation

The current action digest does not include the complete declared output map. Changing only a
logical output name or destination may therefore collide with an older result. The output map
must enter identity before Anneal makes a strong shared-cache promise.

## 5. Execution and cache policy

Execution mode and cache policy are separate:

| Execution mode | Current boundary | Cacheability |
|---|---|---|
| `Sealed` | Declared platform sandbox | May be cacheable, subject to policy |
| `Permeable` | Scrubbed environment without OS isolation | Non-cacheable |
| `Native` | Direct host execution | Non-cacheable |

Current cache policies are:

- `Deterministic` — exact action results may be reused.
- `NonCacheable` — always execute.
- `SnapshotBased` — owns persistent state and may also reuse its exact action result.
- `SnapshotConsuming` — restores another action's state but always executes.
- `FixedOutput` — network-capable acquisition whose single output is pinned and verified by an
  expected digest.

Sealing makes hidden reads fail at the platform's enforcement grade. It does not establish that
output bytes are reproducible: a sealed command may still read time, randomness, process state,
or another visible nondeterministic surface.

### Known generic-action deviation

Ordinary actions currently default to `Deterministic`, and `genrule` lowers arbitrary `sh -c`
commands without changing that policy. This contradicts the intended conservative rule contract
for arbitrary commands. Until fixed, generic rules are `Partial`; rule authors must audit every
built-in policy explicitly and must not infer “sealed means deterministic.”

A future reproducibility-verification workflow may provide evidence for promotion, but no such
automatic graduation path exists today.

## 6. Persistent state

Persistent state is a local performance mechanism, not an undeclared artifact channel.

### Phase-separated state

One action produces the state; other actions consume it read-only. pnpm installation state is
the current example. The action model currently supports one state tree per action and does not
structurally enforce one unique producer in every possible graph.

### Interleaved state

The same native tool reads and mutates the state across invocations. Cargo `target/` is the
current example. Declaring interleaved state requires an attestation and epoch because Anneal
must trust the native tool's incremental invalidation. Mutating actions are capped at the local
tier, and bumping the epoch invalidates state created under the older attestation.

Analysis rejects reading interleaved state as an ordinary dependency. Such a read would expose
content absent from the action identity.

### State identity

State keys currently include rule kind, namespace, rule-provided shard values, state kind, and
the interleaved attestation epoch. They do not independently include complete
workspace/package/target identity. Strengthening that boundary is Priority 0 work.

## 7. Snapshot correctness

A snapshot or warm directory may change cost, never declared results. The verification question
is:

> Does execution from managed warm state produce the same declared result as execution after
> discarding that state?

Warm/cold comparison is a one-sided detector: a difference proves a problem, while agreement is
evidence rather than mathematical proof. Interleaved state therefore stays local and revocable.

Snapshots are not portable outputs. Cross-machine reuse belongs in ordinary content-addressed
outputs and ecosystem stores, not `target/` or `node_modules` working trees.

## 8. Rule-author obligations

A first-party or future third-party rule must:

1. Declare every file, generated artifact, environment value, and toolchain root the action may
   read.
2. Declare every output the action is expected to expose.
3. Choose the narrowest input role that matches the product meaning.
4. Consume only configuration axes that affect the action.
5. Use sealed execution for any reusable result.
6. Use fixed-output acquisition for network content and verify the expected digest before CAS
   admission.
7. Treat arbitrary scripts as non-reproducible unless the rule has an explicit safe policy.
8. Declare persistent state by kind and include every non-source factor that defines its valid
   reuse world in the shard.
9. Expose dependencies through providers and action inputs rather than ambient worktree paths.
10. Add cold/warm neutrality and cache-hit equivalence tests for every stateful or cacheable
    action shape.

## 9. Current first-party rules

- `cargo_workspace`
- `pnpm_workspace`
- `nickel_eval`
- `filegroup`
- `alias`
- `genrule`

Their current limitations are summarized in the README. The archived
[long-form rule design](archive/rule-contract-design.md) preserves earlier rationale and the
proposed reproducibility-graduation model; it is not the current implementation contract.
