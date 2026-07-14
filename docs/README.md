# Anneal documentation

Anneal's documentation is organized by authority. A document's category determines how its
claims should be read.

## Sources of truth

1. [`../README.md`](../README.md) is the authoritative current feature and CLI summary.
2. Normative contract documents describe guarantees current code is expected to enforce:
   [`sandbox-contract.md`](sandbox-contract.md) and [`rules.md`](rules.md).
3. [`../DESIGN.md`](../DESIGN.md) explains the current as-built architecture.
4. [`roadmap.md`](roadmap.md) describes intended sequencing, not commitments.
5. Proposal documents describe possible future mechanisms and are not implemented unless the
   README says otherwise.
6. Archive documents preserve rationale and history but are never implementation-status
   references.

When prose conflicts with current code or tests, treat that as a documentation or
implementation defect. Do not resolve it by silently treating a proposal as shipped behavior.

## Document classes

### Product

- [`why-anneal.md`](why-anneal.md) — the product thesis using current capabilities.
- [`roadmap.md`](roadmap.md) — Now / Next / Later sequencing and prerequisites.

### Current architecture and contracts

- [`../DESIGN.md`](../DESIGN.md) — as-built architecture overview.
- [`sandbox-contract.md`](sandbox-contract.md) — normative platform execution boundary.
- [`rules.md`](rules.md) — normative rule/engine responsibilities.
- [`sandboxing.md`](sandboxing.md) — sandbox, snapshot, and warm-state mechanics.
- [`pnpm-workspace.md`](pnpm-workspace.md) — current pnpm rule behavior plus labeled deferrals.

### Decisions

- [`decisions/`](decisions/) — accepted architectural decisions, including their rationale and
  consequences.

### Proposals

- [`proposals/`](proposals/README.md) — proposed mechanisms that are not current product
  commitments.
- [`simplex-rules.md`](simplex-rules.md) — an actively drafted Simplex integration proposal,
  retained at its working path.

### Engineering records

- [`../TODO.md`](../TODO.md) — active open engineering work.
- [`benchmarks/current.md`](benchmarks/current.md) — benchmark method, observations, and the
  performance promise they support.
- [`archive/`](archive/README.md) — superseded plans, design conversations, and investigation
  logs.

## Writing rules

- Only current product, architecture, and contract documents use unqualified present tense.
- Command examples in current documents must work with the CLI listed in the README.
- A proposal begins with `Status: proposed, not implemented` and names its prerequisites.
- A roadmap item is directional unless it names a release commitment explicitly.
- Historical documents carry an archive banner and are not linked as current reference.
- Normative guarantees should link to the tests that establish them.
- Platform guarantees are graded explicitly; `LoudBestEffort` is not described as
  `Enforced`.
- “Available” means implemented, exposed, and tested. Important correctness or operational
  limitations make a capability `Partial` even when its happy path works.
- Future CLI transcripts do not appear in current product documentation. Show future workflows
  as prose or in a clearly labeled proposal.

Suggested header for non-current documents:

```markdown
> **Status:** Proposed, not implemented.
> **Prerequisites:** ...
> **Current behavior:** See the README feature matrix.
```
