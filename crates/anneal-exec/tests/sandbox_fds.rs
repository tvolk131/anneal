#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs::{self, File};
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::process::Command;

use anneal_exec::{Executor, LocalExecutor};

mod support;

fn output_text(exec: &LocalExecutor, result: &anneal_exec::ActionResult, name: &str) -> String {
    let digest = result.outputs.get(name).copied().unwrap();
    String::from_utf8(exec.cas().get(&digest).unwrap().unwrap()).unwrap()
}

fn sealed_sandbox_available() -> bool {
    if cfg!(target_os = "linux") {
        return Command::new("bwrap")
            .arg("--version")
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    }
    true
}

fn fd_path(fd: RawFd) -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from(format!("/proc/self/fd/{fd}"))
    } else {
        PathBuf::from(format!("/dev/fd/{fd}"))
    }
}

struct FdFlagGuard {
    fd: RawFd,
    flags: libc::c_int,
}

impl FdFlagGuard {
    fn make_inheritable(file: &File) -> Self {
        let fd = file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0, "F_GETFD failed");
        let updated = flags & !libc::FD_CLOEXEC;
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, updated) },
            0,
            "F_SETFD failed"
        );
        FdFlagGuard { fd, flags }
    }
}

impl Drop for FdFlagGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::fcntl(self.fd, libc::F_SETFD, self.flags) };
    }
}

struct StdinGuard {
    saved: RawFd,
}

impl StdinGuard {
    fn replace_with(file: &File) -> Self {
        let saved = unsafe { libc::dup(libc::STDIN_FILENO) };
        assert!(saved >= 0, "dup stdin failed");
        assert_eq!(
            unsafe { libc::dup2(file.as_raw_fd(), libc::STDIN_FILENO) },
            libc::STDIN_FILENO,
            "dup2 stdin failed"
        );
        StdinGuard { saved }
    }
}

impl Drop for StdinGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::dup2(self.saved, libc::STDIN_FILENO) };
        let _ = unsafe { libc::close(self.saved) };
    }
}

#[test]
fn sealed_action_does_not_inherit_parent_file_descriptors() {
    if !sealed_sandbox_available() {
        eprintln!("skipping: sealed sandbox backend is not installed");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("parent-fd-secret.txt");
    fs::write(&secret, b"leaked").unwrap();
    let secret_file = File::open(&secret).unwrap();
    let _guard = FdFlagGuard::make_inheritable(&secret_file);
    let inherited_path = fd_path(secret_file.as_raw_fd());

    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let script = format!(
        "if cat {} > out.txt 2>/dev/null; then :; else printf closed > out.txt; fi",
        inherited_path.display()
    );
    let action = support::shell_action("sealed-fd-inheritance", script)
        .output("out", "out.txt")
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "closed");
}

#[test]
fn sealed_action_gets_null_stdin_instead_of_parent_stdin() {
    if !sealed_sandbox_available() {
        eprintln!("skipping: sealed sandbox backend is not installed");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let stdin_file = dir.path().join("parent-stdin.txt");
    fs::write(&stdin_file, b"leaked\n").unwrap();
    let stdin_file = File::open(&stdin_file).unwrap();
    let _guard = StdinGuard::replace_with(&stdin_file);

    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let action = support::shell_action(
        "sealed-null-stdin",
        "if IFS= read -r line; then printf '%s' \"$line\"; else printf empty; fi > out.txt",
    )
    .output("out", "out.txt")
    .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "empty");
}
