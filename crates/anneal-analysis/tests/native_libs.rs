//! `cargo_workspace` `native_libs`, end to end through the sandbox: a workspace
//! whose member links **zlib** — discovered via `pkg-config` exactly as a `-sys`
//! crate would — builds, links, and *runs* inside the sealed sandbox. This is the
//! execution-level proof the analysis-level `native_libs_attach_*` test cannot give:
//! that the attached toolchain's env+roots actually let a real native library be
//! found at build time, linked, and resolved at **run** time (the test binary calls
//! a zlib symbol — if the dylib weren't mounted on the run action it wouldn't start).
//!
//! Network-gated (`ANNEAL_NETWORK_TESTS=1`): fetches the `pkg-config` crate from
//! crates.io. Runs on every platform — zlib is in anneal's manifest unconditionally.
//!   ANNEAL_NETWORK_TESTS=1 cargo test -p anneal-analysis --test native_libs

use anneal_analysis::Analyzer;
use anneal_core::{AxisValues, Configuration, Label, OptLevel, Platform};
use anneal_exec::LocalExecutor;
use anneal_loader::load_package;
use anneal_rules::builtin_rules;

fn gated() -> bool {
    if std::env::var_os("ANNEAL_NETWORK_TESTS").is_none() {
        eprintln!("skipping: set ANNEAL_NETWORK_TESTS=1 to run network-gated native_libs tests");
        return false;
    }
    true
}

/// A workspace whose member links zlib via a `pkg-config` build script and calls a
/// zlib symbol from a unit test. `build` is the BUILD-file body (varies native_libs).
fn zlib_workspace(build: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(ws.join("mylib/src")).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        "[workspace]\nmembers = [\"mylib\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("mylib/Cargo.toml"),
        "[package]\nname = \"mylib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [build-dependencies]\npkg-config = \"0.3\"\n",
    )
    .unwrap();
    // Discover zlib exactly as a -sys crate does: pkg-config reads zlib.pc (found via
    // the PKG_CONFIG_PATH that native_libs=["zlib"] supplies) and emits the -L search
    // path + -lz. Without native_libs the probe finds nothing and panics → build fails.
    std::fs::write(
        ws.join("mylib/build.rs"),
        "fn main() {\n    \
            pkg_config::probe_library(\"zlib\").expect(\"zlib via pkg-config\");\n}\n",
    )
    .unwrap();
    // Calling zlibVersion() forces a real link *and* a runtime symbol resolution, so
    // the unit test passing proves the lib was mounted at both link and run time.
    std::fs::write(
        ws.join("mylib/src/lib.rs"),
        "use std::ffi::CStr;\n\
         use std::os::raw::c_char;\n\
         extern \"C\" {\n    fn zlibVersion() -> *const c_char;\n}\n\
         pub fn zlib_version() -> String {\n    \
             unsafe { CStr::from_ptr(zlibVersion()).to_string_lossy().into_owned() }\n}\n\
         #[cfg(test)]\n\
         mod tests {\n    \
             #[test]\n    \
             fn links_and_calls_zlib() {\n        \
                 assert!(!super::zlib_version().is_empty(), \"zlibVersion returned empty\");\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(ws.join("BUILD"), build).unwrap();

    let ok = std::process::Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&ws)
        .status()
        .expect("cargo available")
        .success();
    assert!(ok, "cargo generate-lockfile failed");
    tmp
}

fn host_debug() -> Configuration {
    Configuration::new(
        Platform::new("host", "host"),
        AxisValues {
            opt_level: OptLevel::Debug,
            ..Default::default()
        },
    )
}

#[test]
fn native_libs_zlib_links_and_runs_through_the_sandbox() {
    if !gated() {
        return;
    }
    let tmp = zlib_workspace("cargo_workspace(name = \"ws\", native_libs = [\"zlib\"])\n");
    let root = tmp.path();
    let registry = builtin_rules();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();
    let graph = load_package(root, "ws", &registry).unwrap();
    let g = Analyzer::new(&graph, &registry, &host_debug(), root, exec.cas())
        .analyze(&Label::parse("//ws:ws").unwrap())
        .unwrap();
    let actions: Vec<_> = g.actions().cloned().collect();
    let results = exec.execute_graph(&actions).unwrap();

    // Every action succeeds: the compile actions found zlib via pkg-config and linked it.
    for (action, result) in actions.iter().zip(&results) {
        assert!(
            result.success(),
            "action {:?} failed (exit {}): native_libs zlib should build+link",
            action.name(),
            result.exit_code
        );
    }

    // The unit test binary *ran* and called zlibVersion() — proving zlib's closure
    // was mounted on the run action too (a dynamically-linked binary that couldn't
    // resolve libz at runtime would not start). The run action always exits 0 and
    // records the inner exit as ANNEAL_TEST_EXIT, so read that, not the action exit.
    let (run_action, run_result) = actions
        .iter()
        .zip(&results)
        .find(|(a, _)| a.name().starts_with("cargo_workspace test-run"))
        .expect("a unit test-run action");
    let digest = run_result
        .outputs
        .get("results.txt")
        .unwrap_or_else(|| panic!("{} produced no results.txt", run_action.name()));
    let results_txt = String::from_utf8(exec.cas().get(digest).unwrap().unwrap()).unwrap();
    assert!(
        results_txt
            .lines()
            .any(|l| l.trim() == "ANNEAL_TEST_EXIT=0"),
        "the zlib unit test must pass at runtime; results.txt was:\n{results_txt}"
    );
}

#[test]
fn without_native_libs_the_zlib_probe_fails() {
    if !gated() {
        return;
    }
    // Same workspace, no native_libs: PKG_CONFIG_PATH is unset in the scrubbed
    // sandbox, the build script's pkg-config probe finds nothing and panics, so the
    // compiling actions fail. This is what makes native_libs *load-bearing* — the
    // positive test isn't passing because zlib leaked in some other way.
    let tmp = zlib_workspace("cargo_workspace(name = \"ws\")\n");
    let root = tmp.path();
    let registry = builtin_rules();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();
    let graph = load_package(root, "ws", &registry).unwrap();
    let g = Analyzer::new(&graph, &registry, &host_debug(), root, exec.cas())
        .analyze(&Label::parse("//ws:ws").unwrap())
        .unwrap();
    let actions: Vec<_> = g.actions().cloned().collect();
    let results = exec.execute_graph(&actions).unwrap();

    let build = actions
        .iter()
        .zip(&results)
        .find(|(a, _)| a.name().starts_with("cargo_workspace build"))
        .expect("a build action");
    assert!(
        !build.1.success(),
        "without native_libs, the zlib pkg-config probe must fail the build"
    );
}
