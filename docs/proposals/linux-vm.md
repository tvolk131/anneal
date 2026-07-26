# Linux VM execution on macOS

> **Status:** Proposed, not implemented.
> **Prerequisites:** production confidence in the Linux sandbox, a measured workload requiring
> stronger macOS enforcement, and a lifecycle/transport design with acceptable developer
> overhead.

A Linux VM could let macOS clients execute selected actions under the same structural Linux
isolation used in CI. This is a hedge for actions that require an `Enforced` grade rather than
macOS Seatbelt's `LoudBestEffort` boundary.

A design must address:

- VM creation, upgrade, recovery, and resource limits;
- CAS and action transport without treating the host worktree as an ambient VM mount;
- stable guest toolchain paths and platform identity;
- cancellation, logs, and sandbox diagnostics;
- warm guest lifetime without making the VM an authority over correctness;
- offline and failure behavior;
- cold-start and incremental latency gates.

The current macOS executor continues to use Seatbelt and is documented in
[`../sandbox-contract.md`](../sandbox-contract.md). Hard filesystem hermeticity currently
requires running Anneal on Linux.
