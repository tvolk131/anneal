//! The sandbox (§7.3, §7.4): OS isolation and environment hermeticity.
//!
//! Answers *what is the action allowed to do?* It builds the [`Command`] that the
//! executor spawns, applying the isolation appropriate to the action's
//! [`ExecutionMode`]. This is the only OS-specific module; it sits behind a `cfg`
//! seam (macOS `sandbox-exec` today, Linux mount-namespaces later).
//!
//! ## What is enforced in Milestone 1
//!
//! * **Environment hermeticity (all platforms, sealed & permeable):** the
//!   environment is cleared and reset to canonical, deterministic values (§7.4);
//!   only `env`-declared variables are added on top. There is no host passthrough.
//! * **Network denial (macOS, sealed):** a `sandbox-exec` profile denies network.
//! * **Strict input-only filesystem visibility:** deferred. macOS `sandbox-exec` is
//!   best-effort (§7.3, §22); Linux kernel-enforced bind mounts come with the Linux
//!   isolation path. `native` mode applies no isolation and inherits the host env.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use crate::action::{Action, ExecutionMode};

/// Everything the sandbox needs that is not on the action itself.
pub(crate) struct SandboxSpec<'a> {
    pub mode: ExecutionMode,
    pub cwd: &'a Path,
    pub home: &'a Path,
    pub tmp: &'a Path,
    pub env: &'a BTreeMap<String, String>,
}

/// `sandbox-exec` profile: allow everything, then deny all network. Best-effort
/// isolation — see the module docs.
#[cfg(target_os = "macos")]
const SEALED_PROFILE: &str = "(version 1)(allow default)(deny network*)";

/// Build the command to spawn for `action` under `spec`.
pub(crate) fn build_command(action: &Action, spec: &SandboxSpec) -> Command {
    let program = &action.command[0];
    let args = &action.command[1..];

    let mut cmd = wrap(spec.mode, program, args);
    cmd.current_dir(spec.cwd);

    match spec.mode {
        ExecutionMode::Sealed | ExecutionMode::Permeable => apply_canonical_env(&mut cmd, spec),
        // Native runs directly with the inherited host environment (§7.2).
        ExecutionMode::Native => {}
    }

    cmd
}

/// Choose the program to actually launch, wrapping in the OS isolation layer for
/// sealed mode where available.
#[cfg(target_os = "macos")]
fn wrap(mode: ExecutionMode, program: &str, args: &[String]) -> Command {
    match mode {
        ExecutionMode::Sealed => {
            let mut cmd = Command::new("/usr/bin/sandbox-exec");
            cmd.arg("-p").arg(SEALED_PROFILE).arg("--").arg(program).args(args);
            cmd
        }
        ExecutionMode::Permeable | ExecutionMode::Native => {
            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn wrap(_mode: ExecutionMode, program: &str, args: &[String]) -> Command {
    // Linux kernel-enforced isolation (mount namespaces) lands with the Linux path;
    // until then, sealed differs from permeable only by env scrubbing + the
    // (future) namespace setup.
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd
}

/// Clear the environment and set canonical, deterministic values (§7.4), then layer
/// the action's declared `env` on top.
fn apply_canonical_env(cmd: &mut Command, spec: &SandboxSpec) {
    cmd.env_clear();
    cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    cmd.env("LANG", "C.UTF-8");
    cmd.env("LC_ALL", "C.UTF-8");
    cmd.env("TZ", "UTC");
    cmd.env("TERM", "dumb");
    cmd.env("USER", "anneal");
    cmd.env("HOSTNAME", "anneal");
    cmd.env("SHELL", "/bin/sh");
    cmd.env("HOME", spec.home);
    cmd.env("TMPDIR", spec.tmp);
    cmd.env("PWD", spec.cwd);

    for (key, value) in spec.env {
        cmd.env(key, value);
    }
}
