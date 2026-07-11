#![cfg(target_os = "macos")]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anneal_core::Digest;
use anneal_exec::{Action, Executor, LocalExecutor, Toolchain};

mod support;

fn output_text(exec: &LocalExecutor, result: &anneal_exec::ActionResult, name: &str) -> String {
    let digest = result.outputs.get(name).copied().unwrap();
    String::from_utf8(exec.cas().get(&digest).unwrap().unwrap()).unwrap()
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[test]
fn sealed_action_cannot_read_undeclared_host_file() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("host-secret.txt");
    fs::write(&secret, b"secret").unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();

    let script = format!(
        "if cat {} >/dev/null 2>&1; then printf leaked; else printf denied; fi > out.txt",
        sh_quote(&secret)
    );
    let action = support::shell_action("macos-host-read", script)
        .output("out", "out.txt")
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "denied");
}

#[test]
fn sealed_action_cannot_write_undeclared_host_file() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("host-write.txt");
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();

    let script = format!(
        "if (printf pwned > {}) 2>/dev/null; then printf leaked; else printf denied; fi > out.txt",
        sh_quote(&target)
    );
    let action = support::shell_action("macos-host-write", script)
        .output("out", "out.txt")
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "denied");
    assert!(!target.exists(), "sandbox wrote to an undeclared host path");
}

#[test]
fn sealed_action_cannot_escape_through_symlink_to_undeclared_host_file() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("host-secret.txt");
    fs::write(&secret, b"secret").unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();

    let script = format!(
        "ln -s {} escape-link\n\
         if cat escape-link >/dev/null 2>&1; then printf leaked; else printf denied; fi > out.txt",
        sh_quote(&secret)
    );
    let action = support::shell_action("macos-symlink-escape", script)
        .output("out", "out.txt")
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "denied");
}

#[test]
fn sealed_action_reports_undeclared_host_metadata_as_visible() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("host-secret.txt");
    fs::write(&secret, b"secret").unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();

    let script = format!(
        "if test -e {}; then printf visible; else printf hidden; fi > out.txt",
        sh_quote(&secret)
    );
    let action = support::shell_action("macos-host-metadata", script)
        .output("out", "out.txt")
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "visible");
}

#[test]
fn sealed_action_can_read_declared_input_and_write_declared_output() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let input = exec.cas().put(b"declared").unwrap();
    let action = support::shell_action("macos-declared-io", "cat in.txt > out.txt")
        .source_input("in", "in.txt", input)
        .output("out", "out.txt")
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "declared");
}

#[test]
fn declared_toolchain_root_is_readable_but_not_writable() {
    let dir = tempfile::tempdir().unwrap();
    let tool_root = dir.path().join("fake-toolchain");
    let bin = tool_root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let tool = bin.join("fake-tool");
    let shell = support::shell_argv("true").remove(0);
    fs::write(
        &tool,
        format!(
            "#!{}\n\
             printf tool-ok > out.txt\n\
             if (printf bad > \"$TOOL_ROOT/touched\") 2>/dev/null; then\n\
               printf writable > root.txt\n\
             else\n\
               printf readonly > root.txt\n\
             fi\n",
            shell
        ),
    )
    .unwrap();
    let mut perms = fs::metadata(&tool).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&tool, perms).unwrap();

    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let toolchain = Toolchain::new(
        "fake",
        format!("fake-tool={}", tool.display()),
        vec![bin.clone()],
        vec![tool_root.clone()],
    )
    .unwrap();
    let path_env = format!("{}:{}", bin.to_string_lossy(), support::system_path_env());
    let action = Action::builder("macos-toolchain", ["fake-tool"])
        .toolchain(toolchain)
        .toolchain(support::system_runtime())
        .env("PATH", path_env)
        .env("TOOL_ROOT", tool_root.to_string_lossy())
        .output("out", "out.txt")
        .output("root", "root.txt")
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "tool-ok");
    assert_eq!(output_text(&exec, &result, "root"), "readonly");
    assert!(!tool_root.join("touched").exists());
}

#[test]
fn sealed_action_gets_private_writable_home_and_tmp() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let action = support::shell_action(
        "macos-private-dirs",
        "printf '%s|%s|%s' \"$HOME\" \"$TMPDIR\" \"$PWD\" > paths.txt\n\
         printf home > \"$HOME/home.txt\"\n\
         printf tmp > \"$TMPDIR/tmp.txt\"\n\
         if test -f \"$HOME/home.txt\" && test -f \"$TMPDIR/tmp.txt\"; then\n\
           printf writable > writable.txt\n\
         else\n\
           printf missing > writable.txt\n\
         fi\n",
    )
    .output("paths", "paths.txt")
    .output("writable", "writable.txt")
    .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    let paths = output_text(&exec, &result, "paths");
    assert!(paths.contains("|"), "{paths}");
    assert_eq!(output_text(&exec, &result, "writable"), "writable");
}

#[test]
fn sealed_action_cannot_connect_to_host_loopback_by_default() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();

    let script = format!(
        "if nc -z 127.0.0.1 {port} >/dev/null 2>&1; then\n\
           printf connected > out.txt\n\
         else\n\
           printf isolated > out.txt\n\
         fi\n"
    );
    let action = support::shell_action("macos-loopback-default", script)
        .output("out", "out.txt")
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "isolated");
}

#[test]
fn fixed_output_action_with_network_capability_can_reach_host_loopback() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();

    let script = format!(
        "if nc -z 127.0.0.1 {port} >/dev/null 2>&1; then\n\
           printf connected > out.txt\n\
         else\n\
           printf isolated > out.txt\n\
         fi\n"
    );
    let expected = Digest::of(b"connected");
    let action = support::shell_action("macos-loopback-fixed-output", script)
        .output("out", "out.txt")
        .fixed_output(expected)
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "connected");
}
