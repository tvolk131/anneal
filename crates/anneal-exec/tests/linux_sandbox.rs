#![cfg(target_os = "linux")]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anneal_core::Digest;
use anneal_exec::{Action, Executor, LocalExecutor, Toolchain};

fn bwrap_available() -> bool {
    Command::new("bwrap")
        .arg("--version")
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn output_text(exec: &LocalExecutor, result: &anneal_exec::ActionResult, name: &str) -> String {
    let digest = result.outputs.get(name).copied().unwrap();
    String::from_utf8(exec.cas().get(&digest).unwrap().unwrap()).unwrap()
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn declared_system_runtime() -> Option<(Toolchain, PathBuf)> {
    let shell = ["/usr/bin/sh", "/usr/bin/bash", "/usr/bin/dash"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())?;
    let bin_dir = shell.parent()?.to_path_buf();
    let mut roots = vec![PathBuf::from("/usr")];
    for root in ["/lib", "/lib64"] {
        let path = PathBuf::from(root);
        if path.exists() {
            roots.push(path);
        }
    }
    let toolchain = Toolchain::new(
        "system-runtime",
        format!("shell={}", shell.display()),
        vec![bin_dir],
        roots,
    )
    .unwrap();
    Some((toolchain, shell))
}

fn declared_bash_runtime() -> Option<(Toolchain, PathBuf)> {
    let shell = ["/usr/bin/bash", "/bin/bash"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())?;
    let bin_dir = shell.parent()?.to_path_buf();
    let mut roots = Vec::new();
    for root in ["/usr", "/bin", "/lib", "/lib64"] {
        let path = PathBuf::from(root);
        if path.exists() {
            roots.push(path);
        }
    }
    let toolchain = Toolchain::new(
        "bash-runtime",
        format!("bash={}", shell.display()),
        vec![bin_dir],
        roots,
    )
    .unwrap();
    Some((toolchain, shell))
}

fn path_env(runtime: &Toolchain) -> String {
    runtime.bin_dirs()[0].to_string_lossy().into_owned()
}

fn host_tempdir() -> Option<tempfile::TempDir> {
    let root = Path::new("/var/tmp");
    if !root.is_dir() {
        return None;
    }
    tempfile::Builder::new()
        .prefix("anneal-linux-sandbox-host-")
        .tempdir_in(root)
        .ok()
}

#[test]
fn sealed_action_cannot_read_host_paths_outside_the_sandbox_root() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);

    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("host-secret.txt");
    fs::write(&secret, b"secret").unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();

    let script = format!(
        "if cat {} >/dev/null 2>&1; then printf leaked; else printf denied; fi > out.txt",
        sh_quote(&secret)
    );
    let action = Action::builder("host-read", [shell_name.as_str(), "-c", &script])
        .toolchain(runtime)
        .env("PATH", path_env)
        .output("out", "out.txt")
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "denied");
}

#[test]
fn sealed_action_cannot_write_host_paths_outside_the_sandbox_root() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let Some(host_dir) = host_tempdir() else {
        eprintln!("skipping: /var/tmp is unavailable");
        return;
    };
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);
    let target = host_dir.path().join("pwned.txt");

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let script = format!(
        "if (printf pwned > {}) 2>/dev/null; then printf leaked; else printf denied; fi > out.txt",
        sh_quote(&target)
    );
    let action = Action::builder("host-write", [shell_name.as_str(), "-c", &script])
        .toolchain(runtime)
        .env("PATH", path_env)
        .output("out", "out.txt")
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "denied");
    assert!(!target.exists(), "sandbox wrote to an undeclared host path");
}

#[test]
fn sealed_action_cannot_escape_through_symlink_to_host_path() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let Some(host_dir) = host_tempdir() else {
        eprintln!("skipping: /var/tmp is unavailable");
        return;
    };
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);
    let secret = host_dir.path().join("secret.txt");
    fs::write(&secret, b"secret").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let script = format!(
        "ln -s {} escape-link\n\
         if cat escape-link >/dev/null 2>&1; then printf leaked; else printf denied; fi > out.txt",
        sh_quote(&secret)
    );
    let action = Action::builder("symlink-escape", [shell_name.as_str(), "-c", &script])
        .toolchain(runtime)
        .env("PATH", path_env)
        .output("out", "out.txt")
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "denied");
}

#[test]
fn sealed_action_gets_a_private_network_namespace_by_default() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let action = Action::builder(
        "netns",
        [
            shell_name.as_str(),
            "-c",
            "if grep -Eq 'eth|en|wlan' /proc/net/dev; then printf leaked; else printf isolated; fi > out.txt",
        ],
    )
    .toolchain(runtime)
    .env("PATH", path_env)
    .output("out", "out.txt")
    .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "isolated");
}

#[test]
fn sealed_action_cannot_connect_to_host_loopback_by_default() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, bash)) = declared_bash_runtime() else {
        eprintln!("skipping: no bash found");
        return;
    };
    let bash_name = bash.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let script = format!(
        "if (exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null; then\n\
           printf connected > out.txt\n\
         else\n\
           printf isolated > out.txt\n\
         fi\n"
    );
    let action = Action::builder("loopback-connect", [bash_name.as_str(), "-c", &script])
        .toolchain(runtime)
        .env("PATH", path_env)
        .output("out", "out.txt")
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "isolated");
}

#[test]
fn fixed_output_action_with_network_capability_can_reach_host_loopback() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, bash)) = declared_bash_runtime() else {
        eprintln!("skipping: no bash found");
        return;
    };
    let bash_name = bash.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let script = format!(
        "if (exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null; then\n\
           printf connected > out.txt\n\
         else\n\
           printf isolated > out.txt\n\
         fi\n"
    );
    let expected = Digest::of(b"connected");
    let action = Action::builder("fixed-output-loopback", [bash_name.as_str(), "-c", &script])
        .toolchain(runtime)
        .env("PATH", path_env)
        .output("out", "out.txt")
        .fixed_output(expected)
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "connected");
}

#[test]
fn sealed_action_only_sees_declared_roots_plus_sandbox_roots() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let action = Action::builder(
        "visible-roots",
        [
            shell_name.as_str(),
            "-c",
            "for p in /bin/sh /etc/passwd /etc/group /etc/shadow /root /var /opt /nix/store; do\n\
               if test -e \"$p\"; then printf '%s\\n' \"$p\"; fi\n\
             done > visible.txt\n",
        ],
    )
    .toolchain(runtime)
    .env("PATH", path_env)
    .output("visible", "visible.txt")
    .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(
        output_text(&exec, &result, "visible"),
        "/etc/passwd\n/etc/group\n"
    );
}

#[test]
fn sealed_action_gets_private_home_tmp_and_work_paths() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let action = Action::builder(
        "private-dirs",
        [
            shell_name.as_str(),
            "-c",
            "printf '%s|%s|%s\\n' \"$HOME\" \"$TMPDIR\" \"$PWD\" > paths.txt\n\
             printf home > \"$HOME/home.txt\"\n\
             printf tmp > \"$TMPDIR/tmp.txt\"\n\
             if test -f /home/anneal/home.txt && test -f /tmp/tmp.txt; then\n\
               printf writable > writable.txt\n\
             else\n\
               printf missing > writable.txt\n\
             fi\n",
        ],
    )
    .toolchain(runtime)
    .env("PATH", path_env)
    .output("paths", "paths.txt")
    .output("writable", "writable.txt")
    .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(
        output_text(&exec, &result, "paths"),
        "/home/anneal|/tmp|/work\n"
    );
    assert_eq!(output_text(&exec, &result, "writable"), "writable");
}

#[test]
fn sealed_action_has_fixed_hostname_and_no_effective_capabilities() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let action = Action::builder(
        "identity-hardening",
        [
            shell_name.as_str(),
            "-c",
            "cat /proc/sys/kernel/hostname > hostname.txt\n\
             cap=missing\n\
             while read key value rest; do\n\
               if test \"$key\" = CapEff:; then cap=\"$value\"; fi\n\
             done < /proc/self/status\n\
             printf '%s' \"$cap\" > cap.txt\n",
        ],
    )
    .toolchain(runtime)
    .env("PATH", path_env)
    .output("hostname", "hostname.txt")
    .output("cap", "cap.txt")
    .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "hostname"), "anneal\n");
    let cap = output_text(&exec, &result, "cap");
    assert_ne!(cap.trim(), "missing");
    assert!(
        cap.trim().chars().all(|c| c == '0'),
        "expected no effective capabilities, got {cap:?}"
    );
}

#[test]
fn sealed_action_has_normalized_uid_gid_and_groups() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let action = Action::builder(
        "normalized-identity",
        [
            shell_name.as_str(),
            "-c",
            "printf 'env_user=%s\\n' \"$USER\" > identity.txt\n\
             printf 'uid=' >> identity.txt\n\
             id -u >> identity.txt\n\
             printf 'gid=' >> identity.txt\n\
             id -g >> identity.txt\n\
             printf 'groups=' >> identity.txt\n\
             id -G >> identity.txt\n",
        ],
    )
    .toolchain(runtime)
    .env("PATH", path_env)
    .output("identity", "identity.txt")
    .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    let identity = output_text(&exec, &result, "identity");
    assert_eq!(
        identity,
        "env_user=anneal\nuid=1000\ngid=1000\ngroups=1000\n"
    );
}

#[test]
fn sealed_action_gets_synthetic_account_files() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let action = Action::builder(
        "synthetic-account",
        [
            shell_name.as_str(),
            "-c",
            "cat /etc/passwd > account.txt\n\
             cat /etc/group >> account.txt\n\
             if (printf bad > /work/.anneal-synthetic-etc/passwd) 2>/dev/null; then\n\
               printf writable > backing.txt\n\
             else\n\
               printf readonly > backing.txt\n\
             fi\n",
        ],
    )
    .toolchain(runtime)
    .env("PATH", path_env)
    .output("account", "account.txt")
    .output("backing", "backing.txt")
    .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(
        output_text(&exec, &result, "account"),
        "anneal:x:1000:1000:Anneal Sandbox:/home/anneal:/bin/sh\nanneal:x:1000:\n"
    );
    assert_eq!(output_text(&exec, &result, "backing"), "readonly");
}

#[test]
fn sealed_action_gets_private_writable_dev_shm() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let Some(host_shm) = Path::new("/dev/shm").is_dir().then(|| {
        tempfile::Builder::new()
            .prefix("anneal-linux-sandbox-shm-")
            .tempdir_in("/dev/shm")
    }) else {
        eprintln!("skipping: /dev/shm is unavailable");
        return;
    };
    let Ok(host_shm) = host_shm else {
        eprintln!("skipping: cannot create host /dev/shm tempdir");
        return;
    };
    let secret = host_shm.path().join("host-secret");
    fs::write(&secret, b"secret").unwrap();
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let script = format!(
        "if test -e {}; then printf leaked; else printf hidden; fi > visible.txt\n\
         if printf sandbox > /dev/shm/anneal-private-file && test -f /dev/shm/anneal-private-file; then\n\
           printf writable > writable.txt\n\
         else\n\
           printf broken > writable.txt\n\
         fi\n",
        sh_quote(&secret)
    );
    let action = Action::builder("private-dev-shm", [shell_name.as_str(), "-c", &script])
        .toolchain(runtime)
        .env("PATH", path_env)
        .output("visible", "visible.txt")
        .output("writable", "writable.txt")
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "visible"), "hidden");
    assert_eq!(output_text(&exec, &result, "writable"), "writable");
    assert!(!Path::new("/dev/shm/anneal-private-file").exists());
}

#[test]
fn sealed_action_gets_a_private_dev_pts_mount() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let action = Action::builder(
        "private-dev-pts",
        [
            shell_name.as_str(),
            "-c",
            "if test -d /dev/pts; then printf visible; else printf missing; fi > pts.txt\n\
             if grep -q ' /dev/pts ' /proc/self/mountinfo; then\n\
               printf mounted > mount.txt\n\
             else\n\
               printf absent > mount.txt\n\
             fi\n",
        ],
    )
    .toolchain(runtime)
    .env("PATH", path_env)
    .output("pts", "pts.txt")
    .output("mount", "mount.txt")
    .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "pts"), "visible");
    assert_eq!(output_text(&exec, &result, "mount"), "mounted");
}

#[test]
fn declared_inputs_are_read_only_and_do_not_corrupt_the_cas() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let digest = exec.cas().put(b"original").unwrap();
    let action = Action::builder(
        "readonly-input",
        [
            shell_name.as_str(),
            "-c",
            "if (printf mutated > input.txt) 2>/dev/null; then\n\
               printf writable > write.txt\n\
             else\n\
               printf readonly > write.txt\n\
             fi\n\
             if rm input.txt 2>/dev/null; then\n\
               printf removed > unlink.txt\n\
             else\n\
               printf retained > unlink.txt\n\
             fi\n",
        ],
    )
    .toolchain(runtime)
    .env("PATH", path_env)
    .source_input("input", "input.txt", digest)
    .output("write", "write.txt")
    .output("unlink", "unlink.txt")
    .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "write"), "readonly");
    assert_eq!(output_text(&exec, &result, "unlink"), "retained");
    assert_eq!(
        exec.cas().get(&digest).unwrap().as_deref(),
        Some(&b"original"[..])
    );
}

#[test]
fn writable_inputs_are_private_copies_and_do_not_corrupt_the_cas() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let digest = exec.cas().put(b"original").unwrap();
    let action = Action::builder(
        "writable-input",
        [
            shell_name.as_str(),
            "-c",
            "printf mutated > input.txt\n\
             cat input.txt > out.txt\n",
        ],
    )
    .toolchain(runtime)
    .env("PATH", path_env)
    .writable_source_input("input", "input.txt", digest)
    .output("out", "out.txt")
    .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "mutated");
    assert_eq!(
        exec.cas().get(&digest).unwrap().as_deref(),
        Some(&b"original"[..])
    );
}

#[test]
fn declared_toolchain_roots_are_visible_but_read_only() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let tool_root = dir.path().join("fake-toolchain");
    let bin = tool_root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let tool = bin.join("fake-tool");
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
            shell.display()
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
    let path_env = format!("{}:{}", bin.to_string_lossy(), path_env(&runtime));
    let action = Action::builder("tool", ["fake-tool"])
        .toolchain(toolchain)
        .toolchain(runtime)
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
fn declared_toolchain_actions_do_not_get_standard_host_mounts() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let action = Action::builder(
        "no-standard-mounts",
        [
            shell_name.as_str(),
            "-c",
            "if test -e /bin/sh; then printf leaked; else printf hidden; fi > out.txt",
        ],
    )
    .toolchain(runtime)
    .env("PATH", path_env)
    .output("out", "out.txt")
    .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(output_text(&exec, &result, "out"), "hidden");
}

#[test]
fn documented_non_hermetic_kernel_and_device_surfaces_are_visible() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let action = Action::builder(
        "documented-non-hermetic-surfaces",
        [
            shell_name.as_str(),
            "-c",
            "printf 'kernel=' > surfaces.txt\n\
             cat /proc/sys/kernel/osrelease >> surfaces.txt\n\
             printf 'cpus=' >> surfaces.txt\n\
             grep -c '^processor' /proc/cpuinfo >> surfaces.txt\n\
             if test -c /dev/null; then printf 'dev_null=visible\\n' >> surfaces.txt; fi\n\
             if test -c /dev/urandom; then printf 'dev_urandom=visible\\n' >> surfaces.txt; fi\n\
             printf 'time=' >> surfaces.txt\n\
             date +%s >> surfaces.txt\n",
        ],
    )
    .toolchain(runtime)
    .env("PATH", path_env)
    .output("surfaces", "surfaces.txt")
    .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    let surfaces = output_text(&exec, &result, "surfaces");
    assert!(surfaces.contains("kernel="), "{surfaces}");
    assert!(surfaces.contains("cpus="), "{surfaces}");
    assert!(surfaces.contains("dev_null=visible"), "{surfaces}");
    assert!(surfaces.contains("dev_urandom=visible"), "{surfaces}");
    assert!(surfaces.contains("time="), "{surfaces}");
}

#[test]
fn documented_proc_mount_and_cgroup_surfaces_are_visible() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap is not installed");
        return;
    }
    let Some((runtime, shell)) = declared_system_runtime() else {
        eprintln!("skipping: no /usr/bin shell found");
        return;
    };
    let shell_name = shell.file_name().unwrap().to_string_lossy().into_owned();
    let path_env = path_env(&runtime);

    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path().join(".anneal")).unwrap();
    let action = Action::builder(
        "documented-proc-surfaces",
        [
            shell_name.as_str(),
            "-c",
            "if test -r /proc/self/mountinfo; then printf 'mountinfo=visible\\n'; fi > proc.txt\n\
             if test -r /proc/self/cgroup; then printf 'cgroup=visible\\n'; fi >> proc.txt\n\
             if grep -q ' /work ' /proc/self/mountinfo; then printf 'work_mount=visible\\n'; fi >> proc.txt\n\
             if grep -q ' /tmp ' /proc/self/mountinfo; then printf 'tmp_mount=visible\\n'; fi >> proc.txt\n",
        ],
    )
    .toolchain(runtime)
    .env("PATH", path_env)
    .output("proc", "proc.txt")
    .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    let proc = output_text(&exec, &result, "proc");
    assert!(proc.contains("mountinfo=visible"), "{proc}");
    assert!(proc.contains("cgroup=visible"), "{proc}");
    assert!(proc.contains("work_mount=visible"), "{proc}");
    assert!(proc.contains("tmp_mount=visible"), "{proc}");
}
