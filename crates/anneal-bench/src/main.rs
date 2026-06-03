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

    // --- single-package change (the canonical incremental case, §20.3 "must beat") ---
    // Both caches are now warm. Edit one crate and rebuild: native cargo recompiles
    // just that crate; Anneal restores the full target/ snapshot, recompiles
    // incrementally, then re-saves the full snapshot. Distinct edits each round so
    // neither side serves a stale cache; take the fastest.
    let mut cargo_change = Duration::MAX;
    let mut anneal_change = Duration::MAX;
    for i in 0..3 {
        edit_one_crate(&ws, i);
        cargo_change = cargo_change.min(timed(1, || assert!(cargo_build(&ws).success(), "cargo incremental failed")));
        anneal_change = anneal_change.min(timed(1, || run_anneal_build(&exec, root, &label)));
    }

    // --- single-package change WITH warm-sandbox reuse (the optimization under test) ---
    // A separate, warm-reuse-enabled store, primed cold once, then the same edit/rebuild
    // loop: the snapshot owner keeps target/ in place and syncs only the changed source,
    // so restore + teardown drop off the critical path.
    let warm_exec = LocalExecutor::new(root.join(".anneal-warm")).expect("open warm store").warm_reuse();
    run_anneal_build(&warm_exec, root, &label); // cold-populate the warm tree
    let mut anneal_change_warm = Duration::MAX;
    for i in 100..103 {
        edit_one_crate(&ws, i);
        anneal_change_warm = anneal_change_warm.min(timed(1, || run_anneal_build(&warm_exec, root, &label)));
    }

    report(n, cargo_cold, cargo_noop, anneal_cold, anneal_warm, cargo_change, anneal_change, anneal_change_warm);

    // --- phase breakdowns: a cold build, then an incremental rebuild, on a fresh,
    // timing-enabled store (isolated from the comparison runs above). ---
    let profile = LocalExecutor::new(root.join(".anneal-profile"))
        .expect("open profile store")
        .record_timings();
    run_anneal_build(&profile, root, &label); // cold (from scratch)
    let cold_phases = profile.take_timings();
    edit_one_crate(&ws, 99); // warm snapshot now exists; this rebuild is incremental
    run_anneal_build(&profile, root, &label);
    let incremental_phases = profile.take_timings();
    report_phases("Cold build", &cold_phases);
    report_phases("Single-package change (incremental)", &incremental_phases);
}

/// Append a unique function to `crate0`'s source — a content change that busts the
/// action cache (and triggers an incremental recompile of exactly that crate).
fn edit_one_crate(ws: &Path, marker: usize) {
    let path = ws.join("crate0/src/lib.rs");
    let mut src = std::fs::read_to_string(&path).unwrap();
    src.push_str(&format!("pub fn touched_{marker}() {{}}\n"));
    std::fs::write(&path, src).unwrap();
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

#[allow(clippy::too_many_arguments)]
fn report(
    n: usize,
    cargo_cold: Duration,
    cargo_noop: Duration,
    anneal_cold: Duration,
    anneal_warm: Duration,
    cargo_change: Duration,
    anneal_change: Duration,
    anneal_change_warm: Duration,
) {
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
        "| Single-package change | {:.1} ms | {:.1} ms | {:+.0}% vs native |",
        ms(anneal_change), ms(cargo_change), pct(anneal_change, cargo_change),
    );
    println!(
        "| Single-package change (warm reuse) | {:.1} ms | {:.1} ms | {:+.0}% vs native |",
        ms(anneal_change_warm), ms(cargo_change), pct(anneal_change_warm, cargo_change),
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
        "\n_Cold & single-change = overhead gates. No-op & fresh-checkout = the cache \
         wins. Warm reuse is the §5 optimization. Library pipeline; excludes CLI startup._"
    );
}

/// Print where a build's wall-clock goes, phase by phase. `run` is the inner cargo
/// invocation (≈ the native baseline); everything else is Anneal's wrapping.
fn report_phases(title: &str, timings: &[PhaseTimings]) {
    let Some(t) = timings.iter().find(|t| t.action.starts_with("cargo_workspace build")) else {
        return;
    };
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let pct = |d: std::time::Duration| d.as_secs_f64() / t.total.as_secs_f64() * 100.0;
    let wrap = t.total.saturating_sub(t.run);

    println!("\n## {title} — phase breakdown (single run)\n");
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
