# Sealed Sandbox Contract

This is the contract rule authors can rely on when they choose
`ExecutionMode::Sealed`. It describes the observable boundary Anneal provides; rule
authors are still responsible for writing actions that are deterministic given this
boundary.

See `docs/sandboxing.md` for the implementation mechanics and warm-sandbox details.

## Execution modes

| Mode | Contract |
|---|---|
| `Sealed` | Hermetic execution boundary. Cacheable actions must use this mode. Linux provides strict filesystem visibility; macOS provides environment hermeticity, network denial, and a Seatbelt filesystem policy, but not Linux-style namespace hermeticity. |
| `Permeable` | Environment is scrubbed like sealed mode, but the process runs without OS filesystem/network isolation. Not cacheable. |
| `Native` | Direct host execution. Inherits the host environment and applies no sandbox isolation. Not cacheable. |

## Linux sealed contract

Linux sealed actions run through `bubblewrap`. A missing or unusable `bwrap` is an
execution error before the action command starts.

The command sees these guest paths:

| Path | Visibility |
|---|---|
| `/work` | The prepared action working tree. Writable except for declared inputs, which are overmounted read-only. `PWD` points here or to a subdirectory under it. |
| `/home/anneal` | Private writable `HOME`. |
| `/tmp` | Private writable `TMPDIR`. |
| `/dev/shm` | Private writable tmpfs. |
| `/etc/passwd`, `/etc/group` | Synthetic Anneal-owned account files for UID/GID `1000`; not host `/etc`. |
| declared toolchain roots | Mounted read-only at their declared absolute paths. |
| `/proc` | Private proc mount, but still exposes kernel/process information. |
| `/dev` | Bubblewrap-created device tree, including private `/dev/pts` and `/dev/shm`. Device availability is not a determinism guarantee. |

Everything else is absent unless it is contained in a declared toolchain root. In
particular, sealed actions do not get implicit access to host `/bin`, host `/etc`,
`/var`, `/opt`, `/root`, or `/nix/store`.

The Linux wrapper also:

- denies network by default with a private network namespace;
- allows network only when the action carries the network capability, used for
  fixed-output fetches;
- disconnects parent stdio and marks inherited file descriptors above stderr
  close-on-exec before launching the sandbox backend;
- sets hostname to `anneal`;
- drops effective Linux capabilities;
- starts a new session;
- isolates PID, IPC, UTS, and user namespaces;
- sets UID, GID, and supplementary groups to `1000`;
- asks bubblewrap to isolate cgroup namespaces when the host supports it.

Current Linux non-goals and visible surfaces:

- Cgroup namespace isolation is best-effort for host compatibility.
- Kernel version, CPU count, wall-clock time, `/proc`, `/proc/self/mountinfo`,
  `/proc/self/cgroup`, `/dev/null`, and `/dev/urandom` remain observable. Rule authors
  must not depend on them unless the rule is treated as non-deterministic or otherwise
  fenced.

## macOS sealed contract

macOS sealed actions use `sandbox-exec`.

Guaranteed:

- environment is scrubbed and rebuilt from canonical values plus declared action env;
- network is denied unless the action carries the network capability;
- parent stdio is disconnected and inherited file descriptors above stderr are marked
  close-on-exec before launching `sandbox-exec`;
- undeclared host file reads and writes are denied by a generated Seatbelt profile;
- the prepared sandbox root, private `HOME`, and private `TMPDIR` are writable;
- declared toolchain roots and a small Darwin runtime allowlist are readable.

Not guaranteed:

- Linux-style mount namespace isolation. Denied host paths may still be visible as
  metadata, and the Darwin runtime allowlist exposes system paths such as `/System`,
  `/Library`, `/usr/lib`, locale/zoneinfo data, and standard device files.
- read-only enforcement for declared input paths via Seatbelt. Store-corruption safety
  on macOS comes from APFS clone/copy materialization plus file permissions, not from
  read-only bind mounts.

Hard filesystem hermeticity still requires running on Linux.

## Environment contract

For `Sealed` and `Permeable`, Anneal clears the inherited host environment, then sets:

| Variable | Value |
|---|---|
| `PATH` | `/usr/bin:/bin:/usr/sbin:/sbin`, unless the action declares `PATH` |
| `LANG` | `C.UTF-8` |
| `LC_ALL` | `C.UTF-8` |
| `TZ` | `UTC` |
| `TERM` | `dumb` |
| `USER` | `anneal` |
| `HOSTNAME` | `anneal` |
| `SHELL` | `/bin/sh` |
| `HOME` | sandbox-private home path |
| `TMPDIR` | sandbox-private temp path |
| `PWD` | sandbox working directory |

Declared action environment variables are layered on top and enter the action cache key.

For Linux sealed actions, bare commands such as `sh` must declare at least one
toolchain/runtime and an explicit `PATH`. Absolute commands must live under a declared
toolchain read-only root. Relative commands with a slash, such as `./tool`, are allowed
only when the executable is present in the prepared working tree.

## Toolchain contract

A `Toolchain` has two jobs:

- identity: its resolved identity enters the action cache key;
- availability: its declared read-only roots are mounted into Linux sealed actions.

First-party rules resolve toolchains only through
`ANNEAL_TOOLCHAIN_MANIFEST`. The default `nix develop` shell exports this
variable, pointing at a Nix-built JSON manifest that declares the exact executable
paths and read-only closure roots for the first-party toolchains: `rust`,
`posix-runtime`, `node`, and `nickel`.

Nix computes the closure outside Anneal's analysis hot path. Anneal parses the
manifest once per process, validates that every required tool path and every
mounted root is under `/nix/store/...`, validates that symlink targets stay within
declared store roots, and derives the toolchain identity from the resolved tool
paths plus closure roots. A toolchain change therefore still changes the action
cache key.

If `ANNEAL_TOOLCHAIN_MANIFEST` is absent, first-party toolchain resolution fails
closed with a configuration error. Anneal does not discover first-party tools from
ambient `PATH` and does not run `nix-store -qR` at analysis time.

The core `Toolchain` type intentionally records absolute roots and identity, but
does not require Nix for all possible callers. Custom callers are responsible for
declaring a complete runtime closure; the Linux sandbox only mounts the roots it is
given.

## Rule author obligations

Rule authors must:

- declare every input the action may read;
- declare every output the action is expected to produce;
- declare toolchain/runtime roots and set an explicit `PATH` for bare commands;
- use fixed-output actions for network fetches and pin the expected digest;
- write rules that are deterministic relative to the documented visible surfaces;
- mark actions non-cacheable when their outputs are not reproducible under this contract.

Sealing makes the cache key trustworthy. It does not by itself prove that outputs are
deterministic.

## Guarantee-to-test map

| Guarantee or documented surface | Linux test |
|---|---|
| Undeclared host files cannot be read | `sealed_action_cannot_read_host_paths_outside_the_sandbox_root` |
| Undeclared host paths cannot be written | `sealed_action_cannot_write_host_paths_outside_the_sandbox_root` |
| Symlinks cannot escape to hidden host paths | `sealed_action_cannot_escape_through_symlink_to_host_path` |
| Network namespace is private by default | `sealed_action_gets_a_private_network_namespace_by_default` |
| Host loopback services cannot be reached by default | `sealed_action_cannot_connect_to_host_loopback_by_default` |
| Fixed-output/network-capable actions can reach host loopback | `fixed_output_action_with_network_capability_can_reach_host_loopback` |
| Parent file descriptors are not inherited | `sealed_action_does_not_inherit_parent_file_descriptors` |
| Parent stdin is not inherited | `sealed_action_gets_null_stdin_instead_of_parent_stdin` |
| Only declared roots and sandbox roots are visible | `sealed_action_only_sees_declared_roots_plus_sandbox_roots` |
| Private home, temp, and work paths are stable | `sealed_action_gets_private_home_tmp_and_work_paths` |
| Hostname is fixed and effective capabilities are dropped | `sealed_action_has_fixed_hostname_and_no_effective_capabilities` |
| UID/GID/group identity is normalized | `sealed_action_has_normalized_uid_gid_and_groups` |
| Synthetic `/etc/passwd` and `/etc/group` describe the normalized account | `sealed_action_gets_synthetic_account_files` |
| `/dev/shm` is private and writable | `sealed_action_gets_private_writable_dev_shm` |
| `/dev/pts` is private inside the device tree | `sealed_action_gets_a_private_dev_pts_mount` |
| Declared inputs are read-only and cannot corrupt the CAS | `declared_inputs_are_read_only_and_do_not_corrupt_the_cas` |
| Declared toolchain roots are visible but read-only | `declared_toolchain_roots_are_visible_but_read_only` |
| No implicit standard host mounts are provided | `declared_toolchain_actions_do_not_get_standard_host_mounts` |
| Known non-hermetic kernel/device/time surfaces remain visible | `documented_non_hermetic_kernel_and_device_surfaces_are_visible` |
| Known non-hermetic `/proc` mount/cgroup surfaces remain visible | `documented_proc_mount_and_cgroup_surfaces_are_visible` |

| Guarantee or documented surface | macOS test |
|---|---|
| Undeclared host files cannot be read | `sealed_action_cannot_read_undeclared_host_file` |
| Undeclared host paths cannot be written | `sealed_action_cannot_write_undeclared_host_file` |
| Symlinks cannot escape to denied host paths | `sealed_action_cannot_escape_through_symlink_to_undeclared_host_file` |
| Denied host paths may still be visible as metadata | `sealed_action_reports_undeclared_host_metadata_as_visible` |
| Declared inputs and outputs work under the Seatbelt profile | `sealed_action_can_read_declared_input_and_write_declared_output` |
| Declared toolchain roots are readable but not writable | `declared_toolchain_root_is_readable_but_not_writable` |
| Private home and temp paths are writable | `sealed_action_gets_private_writable_home_and_tmp` |
| Network is denied by default | `sealed_action_cannot_connect_to_host_loopback_by_default` |
| Fixed-output/network-capable actions can reach host loopback | `fixed_output_action_with_network_capability_can_reach_host_loopback` |
| Parent file descriptors are not inherited | `sealed_action_does_not_inherit_parent_file_descriptors` |
| Parent stdin is not inherited | `sealed_action_gets_null_stdin_instead_of_parent_stdin` |
