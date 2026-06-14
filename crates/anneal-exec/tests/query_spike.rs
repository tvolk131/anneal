//! The DESIGN.md §10 spike: both halves of the cargo-metadata bootstrap split
//! (§3.6) run end-to-end through the real executor as [`QuerySpec`] queries.
//!
//! What this proves, per the spike's predicted pressure points:
//! - **stdout capture**: `cargo metadata` JSON (which can exceed a pipe buffer)
//!   is captured, cached in the CAS, and replayed on cache hits.
//! - **sandbox path stability**: metadata output embeds absolute paths
//!   (`workspace_root`, `manifest_path`), and the early-cutoff keystone — an
//!   input edit that leaves the output byte-identical — holds because the query
//!   sandbox root is keyed by query *identity*, not input digests.
//!
//! Requires a host Rust toolchain (resolved from the cargo that built this
//! test); skips gracefully if none is found.

use std::fs;
use std::path::{Path, PathBuf};

use anneal_exec::{LocalExecutor, QuerySpec, Toolchain};

/// Resolve a sealed-mountable rust toolchain. Preferred source is the Nix
/// manifest (`ANNEAL_TOOLCHAIN_MANIFEST`, exported by the dev shell) because it
/// carries the **closure** as `read_only_roots` — a nix cargo dynamically links
/// dylibs from sibling store paths, which a parent-dir-only mount can't see
/// (the sandbox correctly blocks them). Fallback for non-nix hosts: the cargo
/// that compiled this test (`env!("CARGO")` — under rustup that is the real
/// proxied binary, not the shim) with its toolchain tree as the root.
fn rust_toolchain() -> Option<Toolchain> {
    if let Ok(manifest_path) = std::env::var("ANNEAL_TOOLCHAIN_MANIFEST") {
        let text = fs::read_to_string(&manifest_path).ok()?;
        let manifest: serde_json::Value = serde_json::from_str(&text).ok()?;
        let rust = &manifest["toolchains"]["rust"];
        let cargo = PathBuf::from(rust["tools"]["cargo"].as_str()?);
        let roots: Vec<PathBuf> = rust["read_only_roots"]
            .as_array()?
            .iter()
            .filter_map(|v| v.as_str().map(PathBuf::from))
            .collect();
        return Toolchain::new(
            "rust",
            format!("cargo={}", cargo.display()),
            vec![cargo.parent()?.to_path_buf()],
            roots,
        )
        .ok();
    }
    let cargo = fs::canonicalize(PathBuf::from(env!("CARGO"))).ok()?;
    let bin_dir = cargo.parent()?.to_path_buf();
    let root = bin_dir.parent()?.to_path_buf();
    Toolchain::new(
        "rust",
        format!("cargo={}", cargo.display()),
        vec![bin_dir],
        vec![root],
    )
    .ok()
}

/// A two-member cargo workspace (`b` depends on `a` by path) written to disk,
/// so the full-resolution query has real dependency edges to resolve.
fn write_fixture(dir: &Path) {
    let write = |rel: &str, content: &str| {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    };
    write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
    );
    write(
        "a/Cargo.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write("a/src/lib.rs", "pub fn a() {}\n");
    write(
        "b/Cargo.toml",
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\na = { path = \"../a\" }\n",
    );
    write("b/src/lib.rs", "pub fn b() { a::a() }\n");
}

/// Ingest every fixture file into the executor's CAS and declare it as a query
/// input at its workspace-relative path.
fn metadata_query(
    exec: &LocalExecutor,
    fixture: &Path,
    name: &str,
    extra_args: &[&str],
) -> QuerySpec {
    let toolchain = rust_toolchain().expect("checked by caller");
    let mut args = vec![
        "cargo".to_owned(),
        "metadata".to_owned(),
        "--format-version".to_owned(),
        "1".to_owned(),
        "--offline".to_owned(),
    ];
    args.extend(extra_args.iter().map(|s| (*s).to_owned()));

    // PATH = the toolchain bin dirs, nothing else — sealed actions reject PATH
    // entries outside declared toolchain roots, and `cargo metadata` spawns no
    // system tools.
    let path_env = toolchain
        .bin_dirs()
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");

    let mut builder = QuerySpec::builder(name, args)
        .toolchain(toolchain)
        .env("PATH", path_env)
        .timeout_ms(120_000);
    for rel in [
        "Cargo.toml",
        "a/Cargo.toml",
        "a/src/lib.rs",
        "b/Cargo.toml",
        "b/src/lib.rs",
    ] {
        let digest = exec.cas().ingest_file(&fixture.join(rel)).unwrap();
        builder = builder.input(rel, rel, digest);
    }
    builder.build().unwrap()
}

fn host_ready() -> bool {
    rust_toolchain().is_some()
}

#[test]
fn bootstrap_query_no_deps() {
    if !host_ready() {
        eprintln!("skipping: no host rust toolchain");
        return;
    }
    let store = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    write_fixture(fixture.path());
    let exec = LocalExecutor::new(store.path()).unwrap();

    // The bootstrap rung of the §3.6 ladder: registry-free, reads only the
    // workspace manifests — exactly the query that feeds CargoFetch's includes.
    let spec = metadata_query(
        &exec,
        fixture.path(),
        "cargo-metadata-no-deps",
        &["--no-deps"],
    );
    let result = exec.run_query(&spec).unwrap();
    assert!(!result.cache_hit);
    let json = String::from_utf8(result.stdout).unwrap();
    assert!(
        json.contains("\"workspace_members\""),
        "metadata JSON expected"
    );
    assert!(json.contains("\"name\":\"a\"") || json.contains("\"name\": \"a\""));
    assert!(json.contains("\"name\":\"b\"") || json.contains("\"name\": \"b\""));
}

#[test]
fn full_query_resolves_and_caches() {
    if !host_ready() {
        eprintln!("skipping: no host rust toolchain");
        return;
    }
    let store = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    write_fixture(fixture.path());
    let exec = LocalExecutor::new(store.path()).unwrap();

    // Full resolution: cargo builds the dependency graph (b -> a) and writes a
    // Cargo.lock into the sandbox as scratch. Registry-backed resolution (the
    // phase-separated `Read` rung) arrives with the state-taxonomy work.
    let spec = metadata_query(&exec, fixture.path(), "cargo-metadata-full", &[]);
    let first = exec.run_query(&spec).unwrap();
    assert!(!first.cache_hit);
    let json = String::from_utf8_lossy(&first.stdout);
    assert!(
        json.contains("\"resolve\""),
        "full metadata includes a resolve graph"
    );

    let second = exec.run_query(&spec).unwrap();
    assert!(second.cache_hit, "identical query key must hit the cache");
    assert_eq!(first.stdout, second.stdout);
}

#[test]
fn early_cutoff_bytes_stable_across_input_edit() {
    if !host_ready() {
        eprintln!("skipping: no host rust toolchain");
        return;
    }
    let store = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    write_fixture(fixture.path());
    let exec = LocalExecutor::new(store.path()).unwrap();

    let spec = metadata_query(
        &exec,
        fixture.path(),
        "cargo-metadata-cutoff",
        &["--no-deps"],
    );
    let before = exec.run_query(&spec).unwrap();
    assert!(!before.cache_hit);

    // Edit an input in a way that changes its digest but not the tool's output:
    // a TOML comment. The query key changes (so the query re-runs), but the
    // §3.6 keystone demands byte-identical stdout — which only holds if the
    // sandbox root (embedded in manifest_path/workspace_root) is stable across
    // the edit. This is the test that a per-key or per-run root would fail.
    let manifest = fixture.path().join("Cargo.toml");
    let mut text = fs::read_to_string(&manifest).unwrap();
    text.push_str("# a comment that changes the digest, not the metadata\n");
    fs::write(&manifest, text).unwrap();

    let spec = metadata_query(
        &exec,
        fixture.path(),
        "cargo-metadata-cutoff",
        &["--no-deps"],
    );
    let after = exec.run_query(&spec).unwrap();
    assert!(!after.cache_hit, "digest changed, so the query must re-run");
    assert_eq!(
        String::from_utf8_lossy(&before.stdout),
        String::from_utf8_lossy(&after.stdout),
        "identical output bytes across the edit: the early-cutoff keystone"
    );
}

#[test]
fn metadata_embeds_sandbox_paths() {
    if !host_ready() {
        eprintln!("skipping: no host rust toolchain");
        return;
    }
    let store = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    write_fixture(fixture.path());
    let exec = LocalExecutor::new(store.path()).unwrap();

    let spec = metadata_query(
        &exec,
        fixture.path(),
        "cargo-metadata-paths",
        &["--no-deps"],
    );
    let result = exec.run_query(&spec).unwrap();
    let json = String::from_utf8_lossy(&result.stdout);

    // The phenomenon the stable root exists for: tool output embeds absolute
    // paths. On Linux the sandbox binds the root at the fixed guest path /work,
    // so outputs are machine-independent; on macOS the host path (under the
    // store's queries/ dir) leaks into the output — stable per checkout, not
    // across machines (DESIGN.md §3.6 / §2.8 consumer asymmetry).
    if cfg!(target_os = "linux") {
        assert!(json.contains("/work"), "Linux guest path embedded");
    } else {
        let queries_dir = store.path().join("queries");
        assert!(
            json.contains(&*queries_dir.to_string_lossy()),
            "macOS host query root embedded in output"
        );
    }
}
