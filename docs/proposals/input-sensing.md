# Input sensing

> **Status:** Proposed, not implemented.
> **Prerequisites:** stable diagnostic schema, trace backend evaluation, and explicit rules for
> mapping observed reads to repository-owned declarations.

Input sensing would observe native-tool reads to help authors discover declarations, explain
sandbox failures, and identify coarse dependency edges.

The governing boundary is:

> Observation proposes; declarations govern; the sandbox enforces.

A trace must never silently widen an action's allowed inputs or become execution-time truth.
Results are advisory because host and tool behavior may be incomplete, platform-specific, or
non-deterministic.

A future design must define:

- Linux and macOS observation backends and their enforcement grades;
- one stable normalized event schema;
- repository ownership and ignore rules;
- how reads map to additive source-footprint suggestions;
- redaction and privacy for host paths;
- diagnostics for reads that cannot be attributed safely;
- reproducible tests demonstrating that sensing cannot relax the sealed contract.

No `anneal sense` command exists today.
