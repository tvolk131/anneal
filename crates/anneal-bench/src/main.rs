//! A simple, local benchmark of `cargo_workspace` against native `cargo` (§20).
//!
//! It drives the **library** pipeline (`load_package → Analyzer → execute_graph`),
//! not the `anneal` CLI process, so it isolates build-system work from process
//! startup. The native baseline runs the *exact* cargo invocation the rule wraps
//! (`cargo build --offline --locked --workspace`, `CARGO_INCREMENTAL=0`), so the only
//! thing measured is Anneal's wrapping: sandboxing, content-addressing, and the
//! `target/` snapshot.
//!
//! Four measurements over a fixture of N dependency-free crates yield three gates:
//!   * **Cold build** — anneal-cold vs cargo-cold: the wrapping *overhead* (§20.3 must
//!     "match within margin").
//!   * **No-op rebuild** — anneal cache-hit vs cargo's fingerprint no-op.
//!   * **Fresh checkout, warm cache** — anneal cache-hit vs cargo-from-scratch: the
//!     locally-measurable form of the CI cold-start *beat* (§20.3). A populated
//!     `.anneal/` restores content-addressed outputs while cargo, with no `target/`,
//!     must rebuild. (Remote cache only adds *sharing* of `.anneal/` across machines;
//!     the restore-vs-rebuild win is visible with a local store.)
//!
//! Run with `cargo run -p anneal-bench --release [-- N]` (default N=8).

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anneal_analysis::Analyzer;
use anneal_core::{AxisValues, Configuration, Label, OptLevel, Platform};
use anneal_exec::{Action, LocalExecutor, PhaseTimings};
use anneal_loader::load_package;
use anneal_rules::builtin_rules;

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(8);

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let ws = root.join("ws");
    make_fixture(&ws, n);

    // --- native cargo baseline (same invocation the rule wraps) ---
    clean_target(&ws);
    let cargo_cold = timed(1, || assert!(cargo_build(&ws).success(), "cargo cold build failed"));
    let cargo_noop = timed(3, || assert!(cargo_build(&ws).success(), "cargo no-op build failed"));

    // --- anneal (library pipeline; the build action only, to mirror `cargo build`) ---
    let label = Label::parse("//ws:ws").unwrap();
    let exec = LocalExecutor::new(root.join(".anneal")).expect("open .anneal");
    let anneal_cold = timed(1, || run_anneal_build(&exec, root, &label));
    let anneal_warm = timed(3, || run_anneal_build(&exec, root, &label));

    report(n, cargo_cold, cargo_noop, anneal_cold, anneal_warm);

    // --- phase breakdown of a single cold build (fresh, timing-enabled store) ---
    let profile_exec = LocalExecutor::new(root.join(".anneal-profile"))
        .expect("open profile store")
        .record_timings();
    run_anneal_build(&profile_exec, root, &label);
    report_phases(&profile_exec.take_timings());
}

/// Run the full Anneal build pipeline for `//ws:ws`, executing just the coarse
/// `cargo_workspace build` action so the comparison mirrors `cargo build`.
fn run_anneal_build(exec: &LocalExecutor, root: &Path, label: &Label) {
    let registry = builtin_rules();
    let graph = load_package(root, "ws", &registry).expect("load_package");
    let cfg = Configuration::new(
        Platform::new("host", "host"),
        AxisValues { opt_level: OptLevel::Debug, ..Default::default() },
    );
    let analyzed = Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
        .analyze(label)
        .expect("analyze");
    let build: Vec<Action> = analyzed
        .actions()
        .filter(|a| a.name().starts_with("cargo_workspace build"))
        .cloned()
        .collect();
    assert_eq!(build.len(), 1, "expected exactly one build action");
    let results = exec.execute_graph(&build).expect("execute_graph");
    assert!(results.iter().all(|r| r.success()), "anneal build action failed");
}

/// `cargo build` with the rule's flags/env, output suppressed.
fn cargo_build(ws: &Path) -> std::process::ExitStatus {
    Command::new("cargo")
        .args(["build", "--offline", "--locked", "--workspace"])
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_TERM_COLOR", "never")
        .current_dir(ws)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run cargo")
}

fn clean_target(ws: &Path) {
    let _ = std::fs::remove_dir_all(ws.join("target"));
}

/// Run `f` `reps` times, returning the fastest (least-noisy) wall-clock time.
fn timed(reps: usize, mut f: impl FnMut()) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..reps {
        let start = Instant::now();
        f();
        best = best.min(start.elapsed());
    }
    best
}

/// A workspace of `n` dependency-free crates, plus a `BUILD` and a generated lockfile.
fn make_fixture(ws: &Path, n: usize) {
    let members = (0..n).map(|i| format!("\"crate{i}\"")).collect::<Vec<_>>().join(", ");
    std::fs::create_dir_all(ws).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        format!("[workspace]\nmembers = [{members}]\nresolver = \"2\"\n"),
    )
    .unwrap();
    for i in 0..n {
        let src = ws.join(format!("crate{i}")).join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            ws.join(format!("crate{i}/Cargo.toml")),
            format!("[package]\nname = \"crate{i}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        std::fs::write(
            src.join("lib.rs"),
            format!(
                "pub const ID: usize = {i};\n\
                 pub fn add(a: i64, b: i64) -> i64 {{ a + b }}\n\
                 pub fn mul(a: i64, b: i64) -> i64 {{ a * b }}\n\
                 pub fn describe() -> String {{ format!(\"crate {{}}\", ID) }}\n"
            ),
        )
        .unwrap();
    }
    std::fs::write(ws.join("BUILD"), "cargo_workspace(name = \"ws\")\n").unwrap();
    let ok = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(ws)
        .status()
        .expect("cargo available")
        .success();
    assert!(ok, "cargo generate-lockfile failed");
}

fn report(n: usize, cargo_cold: Duration, cargo_noop: Duration, anneal_cold: Duration, anneal_warm: Duration) {
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let pct = |a: Duration, base: Duration| (a.as_secs_f64() - base.as_secs_f64()) / base.as_secs_f64() * 100.0;
    let times = |slow: Duration, fast: Duration| slow.as_secs_f64() / fast.as_secs_f64();

    println!("# Anneal vs native cargo — {n} dependency-free crates, debug profile\n");
    println!("| Scenario | Anneal | Native cargo | Result |");
    println!("|---|---:|---:|---|");
    println!(
        "| Cold build | {:.0} ms | {:.0} ms | {:+.0}% vs native |",
        ms(anneal_cold), ms(cargo_cold), pct(anneal_cold, cargo_cold),
    );
    println!(
        "| No-op rebuild | {:.1} ms | {:.1} ms | {:.1}× faster |",
        ms(anneal_warm), ms(cargo_noop), times(cargo_noop, anneal_warm),
    );
    println!(
        "| Fresh checkout, warm cache | {:.1} ms | {:.0} ms | {:.1}× faster |",
        ms(anneal_warm), ms(cargo_cold), times(cargo_cold, anneal_warm),
    );
    println!(
        "\n_Cold = overhead gate (must match within margin). No-op & fresh-checkout = \
         the cache wins. Library pipeline; excludes CLI process startup._"
    );
}

/// Print where a cold build's wall-clock goes, phase by phase. `run` is the inner
/// cargo invocation (≈ the native baseline); everything else is Anneal's wrapping.
fn report_phases(timings: &[PhaseTimings]) {
    let Some(t) = timings.iter().find(|t| t.action.starts_with("cargo_workspace build")) else {
        return;
    };
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let pct = |d: std::time::Duration| d.as_secs_f64() / t.total.as_secs_f64() * 100.0;
    let wrap = t.total.saturating_sub(t.run);

    println!("\n## Cold-build phase breakdown (single run)\n");
    println!("| Phase | Time | % of total |");
    println!("|---|---:|---:|");
    for (name, d) in [
        ("materialize inputs", t.materialize),
        ("restore snapshot", t.restore),
        ("run (cargo itself)", t.run),
        ("capture outputs", t.capture),
        ("save target/ snapshot", t.save),
        ("teardown sandbox", t.teardown),
    ] {
        println!("| {name} | {:.1} ms | {:.0}% |", ms(d), pct(d));
    }
    println!("| **total** | **{:.1} ms** | 100% |", ms(t.total));
    println!(
        "\n_Wrapping overhead (total − run) = {:.1} ms ({:.0}% of total). \
         The hypothesis: `save target/ snapshot` dominates it._",
        ms(wrap),
        pct(wrap),
    );
}
