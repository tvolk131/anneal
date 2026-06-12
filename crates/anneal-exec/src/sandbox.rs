//! The sandbox (§7.3, §7.4): OS isolation and environment hermeticity.
//!
//! Answers *what is the action allowed to do?* It builds the [`Command`] that the
//! executor spawns, applying the isolation appropriate to the action's
//! [`ExecutionMode`]. This is the only OS-specific module; it sits behind a `cfg`
//! seam (macOS `sandbox-exec`, Linux `bubblewrap` namespaces and bind mounts).
//!
//! ## What is enforced
//!
//! * **Environment hermeticity (all platforms, sealed & permeable):** the
//!   environment is cleared and reset to canonical, deterministic values (§7.4);
//!   only `env`-declared variables are added on top. There is no host passthrough.
//! * **Network denial (sealed):** Linux sealed actions get a private network
//!   namespace; macOS sealed actions use a `sandbox-exec` profile that denies network —
//!   **unless** the action carries the network capability (`Action::allows_network`),
//!   as a fixed-output fetch does (§FOD), where the output hash fences the impurity.
//! * **Strict filesystem visibility (Linux sealed):** `bubblewrap` exposes the
//!   prepared work tree, private `HOME`/`TMPDIR`, synthetic `/etc/passwd` and
//!   `/etc/group`, `/proc`, `/dev`, and declared toolchain roots only. Declared
//!   inputs are overmounted read-only inside `/work`. Linux sealed actions also drop
//!   effective capabilities, run in a new session, get a private `/dev/shm`, require
//!   a user namespace with a fixed uid/gid, and try to isolate cgroup namespaces when
//!   the host supports it.
//! * **Filesystem visibility (macOS sealed):** `sandbox-exec` applies a generated
//!   Seatbelt profile that denies network by default and denies undeclared host file
//!   reads/writes, while allowing the prepared sandbox, declared toolchains, and a
//!   small Darwin runtime surface. This is not Linux-style namespace isolation.
//!   `native` mode applies no isolation and inherits the host env.
//!
//! The concise rule-author contract is `docs/sandbox-contract.md`; this module owns
//! the local OS-specific implementation.

use std::collections::BTreeMap;
use std::io;
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::action::{Action, ExecutionMode};
use crate::executor::SandboxError;

#[cfg(target_os = "linux")]
use std::ffi::OsStr;

/// The enforcement grade this host's **sealed** backend delivers (DESIGN.md
/// §2.8). A platform fact, not an action property: Linux bubblewrap is
/// structural absence (and a missing/unusable bwrap fails the build rather
/// than degrading, so `Enforced` is accurate whenever a sealed action actually
/// runs); macOS Seatbelt is loud policy interception; the no-backend cfg
/// fallback applies no isolation at all.
pub(crate) fn sealed_enforcement_grade() -> crate::trust::EnforcementGrade {
    if cfg!(target_os = "linux") {
        crate::trust::EnforcementGrade::Enforced
    } else if cfg!(target_os = "macos") {
        crate::trust::EnforcementGrade::LoudBestEffort
    } else {
        crate::trust::EnforcementGrade::Unenforced
    }
}

/// Everything the sandbox needs that is not on the action itself.
pub(crate) struct SandboxSpec<'a> {
    pub mode: ExecutionMode,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub root: &'a Path,
    pub cwd: &'a Path,
    pub home: &'a Path,
    pub tmp: &'a Path,
    pub env: &'a BTreeMap<String, String>,
}

struct EnvPaths {
    cwd: PathBuf,
    home: PathBuf,
    tmp: PathBuf,
}

#[cfg(target_os = "linux")]
const GUEST_ROOT: &str = "/work";
#[cfg(target_os = "linux")]
const GUEST_HOME: &str = "/home/anneal";
#[cfg(target_os = "linux")]
const GUEST_TMP: &str = "/tmp";
#[cfg(target_os = "linux")]
const SANDBOX_UID: &str = "1000";
#[cfg(target_os = "linux")]
const SANDBOX_GID: &str = "1000";
#[cfg(target_os = "linux")]
const SYNTHETIC_ETC_DIR: &str = ".anneal-synthetic-etc";

/// Build the command to spawn for `action` under `spec`.
pub(crate) fn build_command(action: &Action, spec: &SandboxSpec) -> Result<Command, SandboxError> {
    let mut cmd = wrap(action, spec)?;

    match spec.mode {
        ExecutionMode::Sealed | ExecutionMode::Permeable => {
            apply_canonical_env(&mut cmd, spec, env_paths(spec));
            apply_sandbox_process_hardening(&mut cmd)?;
        }
        // Native runs directly with the inherited host environment (§7.2).
        ExecutionMode::Native => {}
    }

    Ok(cmd)
}

fn apply_sandbox_process_hardening(cmd: &mut Command) -> Result<(), SandboxError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        let fds = inherited_fds_to_cloexec()?;
        unsafe {
            cmd.pre_exec(move || mark_fds_cloexec(&fds));
        }
    }

    Ok(())
}

#[cfg(unix)]
fn inherited_fds_to_cloexec() -> Result<Vec<RawFd>, SandboxError> {
    let mut fds = Vec::new();
    for entry in std::fs::read_dir(fd_directory()).map_err(process_hardening_error)? {
        let entry = entry.map_err(process_hardening_error)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(fd) = name.parse::<RawFd>() else {
            continue;
        };
        if fd > libc::STDERR_FILENO {
            fds.push(fd);
        }
    }
    fds.sort_unstable();
    fds.dedup();
    Ok(fds)
}

#[cfg(unix)]
fn fd_directory() -> &'static Path {
    if cfg!(target_os = "linux") {
        Path::new("/proc/self/fd")
    } else {
        Path::new("/dev/fd")
    }
}

#[cfg(unix)]
fn mark_fds_cloexec(fds: &[RawFd]) -> io::Result<()> {
    for &fd in fds {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EBADF) {
                continue;
            }
            return Err(error);
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn process_hardening_error(error: io::Error) -> SandboxError {
    SandboxError::ProcessHardeningFailed {
        message: error.to_string(),
    }
}

/// Choose the program to actually launch, wrapping in the OS isolation layer for
/// sealed mode where available.
#[cfg(target_os = "macos")]
fn wrap(action: &Action, spec: &SandboxSpec) -> Result<Command, SandboxError> {
    let program = &action.command[0];
    let args = &action.command[1..];

    let mut cmd = match spec.mode {
        ExecutionMode::Sealed => {
            let profile = macos_profile(action, spec);
            let mut cmd = Command::new("/usr/bin/sandbox-exec");
            cmd.arg("-p").arg(profile).arg("--").arg(program).args(args);
            cmd
        }
        ExecutionMode::Permeable | ExecutionMode::Native => {
            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd
        }
    };
    cmd.current_dir(spec.cwd);
    Ok(cmd)
}

#[cfg(target_os = "macos")]
fn macos_profile(action: &Action, spec: &SandboxSpec) -> String {
    let mut read_paths = Vec::new();
    let mut write_paths = Vec::new();

    // The dynamic loader and /bin/sh read the root directory itself; this does not
    // grant recursive host reads.
    read_paths.push(PathBuf::from("/"));

    for path in [
        "/System",
        "/Library",
        "/usr/lib",
        "/usr/share",
        "/private/var/db",
        "/private/var/select",
        // Apple's LibreSSL reads /private/etc/ssl/openssl.cnf at library init
        // (and ignores OPENSSL_CONF), so anything linking system libcurl —
        // rustup-distributed cargo, git — aborts without it. Same near-constant
        // Darwin runtime class as /private/var/select above.
        "/private/etc/ssl",
        "/Library/Apple/usr/libexec/oah",
        "/System/Library/Apple/usr/libexec/oah",
        "/System/Library/LaunchDaemons/com.apple.oahd.plist",
        "/Library/Apple/System/Library/LaunchDaemons/com.apple.oahd.plist",
    ] {
        push_existing_path(&mut read_paths, Path::new(path));
    }

    push_path_and_canonical(&mut read_paths, spec.root);
    push_path_and_canonical(&mut write_paths, spec.root);
    push_path_and_canonical(&mut read_paths, spec.home);
    push_path_and_canonical(&mut write_paths, spec.home);
    push_path_and_canonical(&mut read_paths, spec.tmp);
    push_path_and_canonical(&mut write_paths, spec.tmp);

    for toolchain in action.toolchains.values() {
        for root in toolchain.read_only_roots() {
            push_path_and_canonical(&mut read_paths, root);
        }
    }

    let mut profile = String::from(
        r#"(version 1)
(deny default)
(deny file-write-setugid)
(allow process*)
(allow sysctl-read)
(allow ipc-posix*)
(allow ipc-sysv*)
(allow system-socket)
(allow signal (target same-sandbox))
(allow mach-lookup)
(allow file-read-metadata (subpath "/"))
(allow file*
       (literal "/dev/null")
       (literal "/dev/random")
       (literal "/dev/stderr")
       (literal "/dev/stdin")
       (literal "/dev/stdout")
       (literal "/dev/tty")
       (literal "/dev/urandom")
       (literal "/dev/zero")
       (literal "/dev/dtracehelper")
       (subpath "/dev/fd"))
(allow file*
       (literal "/dev/ptmx")
       (regex #"^/dev/pty[a-z]+")
       (regex #"^/dev/ttys[0-9]+"))
"#,
    );

    if action.allows_network() {
        profile.push_str(
            r#"(allow network*)
(allow file-read-metadata
       (literal "/etc")
       (literal "/etc/hosts")
       (literal "/etc/resolv.conf")
       (literal "/private/etc")
       (literal "/private/etc/hosts")
       (literal "/private/etc/resolv.conf")
       (literal "/private/var/run/resolv.conf"))
(allow file-read*
       (literal "/private/etc/hosts")
       (literal "/private/var/run/resolv.conf"))
"#,
        );
    }

    append_path_rule(&mut profile, "file-read*", &read_paths);
    append_path_rule(&mut profile, "file-write*", &write_paths);
    profile
}

#[cfg(target_os = "macos")]
fn push_existing_path(paths: &mut Vec<PathBuf>, path: &Path) {
    if path.exists() {
        push_path_and_canonical(paths, path);
    }
}

#[cfg(target_os = "macos")]
fn push_path_and_canonical(paths: &mut Vec<PathBuf>, path: &Path) {
    push_unique_path(paths, path.to_path_buf());
    if let Ok(canonical) = std::fs::canonicalize(path) {
        push_unique_path(paths, canonical);
    }
}

#[cfg(target_os = "macos")]
fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

#[cfg(target_os = "macos")]
fn append_path_rule(profile: &mut String, operation: &str, paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    profile.push_str("(allow ");
    profile.push_str(operation);
    for path in paths {
        profile.push('\n');
        if path == Path::new("/") {
            profile.push_str("       (literal ");
        } else {
            profile.push_str("       (subpath ");
        }
        profile.push_str(&sbpl_string(path));
        profile.push(')');
    }
    profile.push_str(")\n");
}

#[cfg(target_os = "macos")]
fn sbpl_string(path: &Path) -> String {
    let mut out = String::from("\"");
    for ch in path.to_string_lossy().chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(target_os = "linux")]
fn wrap(action: &Action, spec: &SandboxSpec) -> Result<Command, SandboxError> {
    let program = &action.command[0];
    let args = &action.command[1..];

    let cmd = match spec.mode {
        ExecutionMode::Sealed => {
            let bwrap = bwrap_program()?;
            ensure_bwrap_works(&bwrap, action.allows_network())?;
            let synthetic_etc = prepare_synthetic_etc(spec.root)?;
            let mut cmd = Command::new(bwrap);
            cmd.arg("--die-with-parent")
                .arg("--unshare-pid")
                .arg("--unshare-ipc")
                .arg("--unshare-uts")
                .arg("--unshare-cgroup-try")
                .arg("--unshare-user")
                .arg("--uid")
                .arg(SANDBOX_UID)
                .arg("--gid")
                .arg(SANDBOX_GID)
                .arg("--new-session")
                .arg("--cap-drop")
                .arg("ALL")
                .arg("--hostname")
                .arg("anneal");
            if !action.allows_network() {
                cmd.arg("--unshare-net");
            }

            cmd.arg("--dir")
                .arg(GUEST_ROOT)
                .arg("--bind")
                .arg(spec.root)
                .arg(GUEST_ROOT)
                .arg("--dir")
                .arg("/home")
                .arg("--dir")
                .arg(GUEST_HOME)
                .arg("--bind")
                .arg(spec.home)
                .arg(GUEST_HOME)
                .arg("--dir")
                .arg(GUEST_TMP)
                .arg("--bind")
                .arg(spec.tmp)
                .arg(GUEST_TMP);

            cmd.arg("--dir")
                .arg("/etc")
                .arg("--ro-bind")
                .arg(synthetic_etc.join("passwd"))
                .arg("/etc/passwd")
                .arg("--ro-bind")
                .arg(synthetic_etc.join("group"))
                .arg("/etc/group");

            for input in action.inputs.values().filter(|input| !input.writable) {
                cmd.arg("--ro-bind")
                    .arg(spec.cwd.join(&input.path))
                    .arg(guest_cwd(spec).join(&input.path));
            }

            for toolchain in action.toolchains.values() {
                for root in toolchain.read_only_roots() {
                    add_parent_dirs(&mut cmd, root);
                    cmd.arg("--ro-bind").arg(root).arg(root);
                }
            }

            cmd.arg("--proc")
                .arg("/proc")
                .arg("--dev")
                .arg("/dev")
                .arg("--tmpfs")
                .arg("/dev/shm")
                .arg("--chdir")
                .arg(guest_cwd(spec))
                .arg("--")
                .arg(program)
                .args(args);
            cmd
        }
        ExecutionMode::Permeable | ExecutionMode::Native => {
            let mut cmd = Command::new(program);
            cmd.args(args).current_dir(spec.cwd);
            cmd
        }
    };
    Ok(cmd)
}

#[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
fn wrap(action: &Action, spec: &SandboxSpec) -> Result<Command, SandboxError> {
    let mut cmd = Command::new(&action.command[0]);
    cmd.args(&action.command[1..]).current_dir(spec.cwd);
    Ok(cmd)
}

#[cfg(target_os = "linux")]
fn prepare_synthetic_etc(root: &Path) -> Result<PathBuf, SandboxError> {
    let etc = root.join(SYNTHETIC_ETC_DIR);
    std::fs::create_dir_all(&etc).map_err(synthetic_etc_error)?;
    let passwd = etc.join("passwd");
    let group = etc.join("group");
    std::fs::write(
        &passwd,
        format!("anneal:x:{SANDBOX_UID}:{SANDBOX_GID}:Anneal Sandbox:{GUEST_HOME}:/bin/sh\n"),
    )
    .map_err(synthetic_etc_error)?;
    std::fs::write(&group, format!("anneal:x:{SANDBOX_GID}:\n")).map_err(synthetic_etc_error)?;
    set_permissions(&passwd, 0o444)?;
    set_permissions(&group, 0o444)?;
    set_permissions(&etc, 0o555)?;
    Ok(etc)
}

#[cfg(target_os = "linux")]
fn set_permissions(path: &Path, mode: u32) -> Result<(), SandboxError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(synthetic_etc_error)
}

#[cfg(target_os = "linux")]
fn synthetic_etc_error(error: io::Error) -> SandboxError {
    SandboxError::SyntheticEtcFailed {
        message: error.to_string(),
    }
}

#[cfg(target_os = "linux")]
fn bwrap_program() -> Result<PathBuf, SandboxError> {
    bwrap_program_from_path(std::env::var_os("PATH").as_deref())
}

#[cfg(target_os = "linux")]
fn bwrap_program_from_path(path: Option<&OsStr>) -> Result<PathBuf, SandboxError> {
    path.and_then(|path| {
        std::env::split_paths(path)
            .map(|dir| dir.join("bwrap"))
            .find(|path| is_executable_file(path))
    })
    .and_then(|path| std::fs::canonicalize(&path).ok().or(Some(path)))
    .ok_or(SandboxError::BubblewrapNotFound)
}

#[cfg(target_os = "linux")]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn ensure_bwrap_works(program: &Path, network_allowed: bool) -> Result<(), SandboxError> {
    use std::sync::OnceLock;

    static STRICT_PROBE: OnceLock<Result<(), SandboxError>> = OnceLock::new();
    static NETWORK_ALLOWED_PROBE: OnceLock<Result<(), SandboxError>> = OnceLock::new();

    let probe = if network_allowed {
        &NETWORK_ALLOWED_PROBE
    } else {
        &STRICT_PROBE
    };
    probe
        .get_or_init(|| probe_bwrap(program, network_allowed))
        .clone()
}

#[cfg(target_os = "linux")]
fn probe_bwrap(program: &Path, network_allowed: bool) -> Result<(), SandboxError> {
    let mut cmd = Command::new(program);
    cmd.arg("--die-with-parent")
        .arg("--unshare-pid")
        .arg("--unshare-ipc")
        .arg("--unshare-uts")
        .arg("--unshare-cgroup-try")
        .arg("--unshare-user")
        .arg("--uid")
        .arg(SANDBOX_UID)
        .arg("--gid")
        .arg(SANDBOX_GID)
        .arg("--new-session")
        .arg("--cap-drop")
        .arg("ALL")
        .arg("--hostname")
        .arg("anneal-probe");
    if !network_allowed {
        cmd.arg("--unshare-net");
    }
    cmd.arg("--ro-bind")
        .arg("/")
        .arg("/")
        .arg("--")
        .arg(program)
        .arg("--version");

    let output = cmd
        .output()
        .map_err(|error| SandboxError::BubblewrapProbeFailed {
            program: program.to_path_buf(),
            status: None,
            stderr: error.to_string(),
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(SandboxError::BubblewrapProbeFailed {
        program: program.to_path_buf(),
        status: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

#[cfg(target_os = "linux")]
fn add_parent_dirs(cmd: &mut Command, path: &Path) {
    let mut cur = PathBuf::from("/");
    for component in path.components().skip(1) {
        cur.push(component.as_os_str());
        if cur == path {
            break;
        }
        if matches!(
            cur.to_str(),
            Some("/tmp" | "/work" | "/home" | "/usr" | "/bin" | "/sbin" | "/lib" | "/lib64")
        ) {
            continue;
        }
        cmd.arg("--dir").arg(&cur);
    }
}

fn env_paths(spec: &SandboxSpec) -> EnvPaths {
    #[cfg(target_os = "linux")]
    if spec.mode == ExecutionMode::Sealed {
        return EnvPaths {
            cwd: guest_cwd(spec),
            home: PathBuf::from(GUEST_HOME),
            tmp: PathBuf::from(GUEST_TMP),
        };
    }

    EnvPaths {
        cwd: spec.cwd.to_path_buf(),
        home: spec.home.to_path_buf(),
        tmp: spec.tmp.to_path_buf(),
    }
}

#[cfg(target_os = "linux")]
fn guest_cwd(spec: &SandboxSpec) -> PathBuf {
    let rel = spec
        .cwd
        .strip_prefix(spec.root)
        .unwrap_or_else(|_| Path::new(""));
    if rel.as_os_str().is_empty() {
        PathBuf::from(GUEST_ROOT)
    } else {
        Path::new(GUEST_ROOT).join(rel)
    }
}

/// Clear the environment and set canonical, deterministic values (§7.4), then layer
/// the action's declared `env` on top.
fn apply_canonical_env(cmd: &mut Command, spec: &SandboxSpec, paths: EnvPaths) {
    cmd.env_clear();
    cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    cmd.env("LANG", "C.UTF-8");
    cmd.env("LC_ALL", "C.UTF-8");
    cmd.env("TZ", "UTC");
    cmd.env("TERM", "dumb");
    cmd.env("USER", "anneal");
    cmd.env("HOSTNAME", "anneal");
    cmd.env("SHELL", "/bin/sh");
    cmd.env("HOME", paths.home);
    cmd.env("TMPDIR", paths.tmp);
    cmd.env("PWD", paths.cwd);

    for (key, value) in spec.env {
        cmd.env(key, value);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn bwrap_resolver_requires_an_executable_on_path() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            bwrap_program_from_path(Some(dir.path().as_os_str())).unwrap_err(),
            SandboxError::BubblewrapNotFound
        );

        let bwrap = dir.path().join("bwrap");
        std::fs::write(&bwrap, b"not executable").unwrap();
        std::fs::set_permissions(&bwrap, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            bwrap_program_from_path(Some(dir.path().as_os_str())).unwrap_err(),
            SandboxError::BubblewrapNotFound
        );

        std::fs::set_permissions(&bwrap, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            bwrap_program_from_path(Some(dir.path().as_os_str())).unwrap(),
            bwrap
        );
    }
}
