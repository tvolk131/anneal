use std::path::PathBuf;

use anneal_exec::{Action, ActionBuilder, Toolchain};

pub fn shell_action(name: impl Into<String>, script: impl Into<String>) -> ActionBuilder {
    Action::builder(name, shell_argv(script))
        .toolchain(system_runtime())
        .env("PATH", system_path_env())
}

pub fn shell_argv(script: impl Into<String>) -> Vec<String> {
    vec![
        shell_path().to_string_lossy().into_owned(),
        "-c".to_owned(),
        script.into(),
    ]
}

pub fn system_runtime() -> Toolchain {
    Toolchain::new(
        "test-system-runtime",
        format!("shell={}", shell_path().display()),
        system_bin_dirs(),
        system_roots(),
    )
    .unwrap()
}

pub fn system_path_env() -> String {
    system_bin_dirs()
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":")
}

fn shell_path() -> PathBuf {
    [
        "/bin/sh",
        "/usr/bin/sh",
        "/bin/bash",
        "/usr/bin/bash",
        "/usr/bin/dash",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
    .and_then(|path| std::fs::canonicalize(&path).ok().or(Some(path)))
    .expect("test host has a POSIX shell")
}

fn system_bin_dirs() -> Vec<PathBuf> {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .iter()
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .collect()
}

fn system_roots() -> Vec<PathBuf> {
    [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/usr/lib",
        "/usr/lib64",
        "/libexec",
    ]
    .iter()
    .map(PathBuf::from)
    .filter(|path| path.exists())
    .collect()
}
