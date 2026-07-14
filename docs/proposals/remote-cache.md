# Remote and shared cache

> **Status:** Proposed, not implemented.
> **Prerequisites:** complete action identity, hardened file digests, explicit generic-action
> cache policy, store lifecycle management, trust admission rules, and poisoning tests.

A future remote cache would share content-addressed action outputs and portable ecosystem
content between CI and developer machines. It would not transport native working snapshots
such as Cargo `target/` or pnpm `node_modules`.

The first implementation should remain deliberately small:

- content-addressed blob upload and download;
- action-result records keyed by the complete action contract;
- provenance and enforcement-grade admission checks;
- namespace and authentication boundaries;
- digest verification on every download;
- bounded retention and observability;
- safe fallback to local execution on absence or service failure.

`Promotable` currently records that a local result may be eligible for a future shared tier. It
does not upload anything.

Remote execution is a separate feature and is not a prerequisite for shared exact results.
