//! The genrule cacheability contract (TODO.md P0 #3, resolved): an arbitrary
//! command is `NonCacheable` by default — the engine cannot assume a shell
//! command's purity — and caching is the explicit `deterministic = True`
//! opt-in claim. A wrong claim poisons the cache with stale entries, so the
//! default sits on the safe side.
//!
//! Asserted behaviorally through the executor (the policy is not public
//! surface): a default genrule executes twice and never reports a cache hit;
//! the opt-in executes once and hits on the second run.
//!
//! Runs under `nix develop` like the other rule tests (genrule resolves the
//! base runtime through the toolchain manifest).

use std::path::Path;

use anneal_core::{AxisValues, Configuration, Label, Platform};
use anneal_exec::{Action, Executor, LocalExecutor};
use anneal_rules::{AttrValue, Attrs, GenRule, Rule, TestContext};

fn host_config() -> Configuration {
    Configuration::new(Platform::new("host", "host"), AxisValues::default())
}

/// Analyze a genrule over a scratch package, resolving sources into the
/// *executor's* CAS so the produced action executes against it directly.
fn analyze_genrule(pkg: &Path, exec: &LocalExecutor, deterministic: bool) -> Action {
    std::fs::write(pkg.join("in.txt"), "input").unwrap();
    let mut builder = Attrs::builder()
        .strings("srcs", ["in.txt"])
        .strings("outs", ["out.txt"])
        .string("cmd", "cp $(SRCS) $(OUTS)");
    if deterministic {
        builder = builder.value("deterministic", AttrValue::Bool(true));
    }
    let attrs = builder.build();
    let config = host_config();
    let tc = TestContext::new();
    let ctx = tc.context(
        Label::parse("//pkg:gen").unwrap(),
        &attrs,
        &config,
        pkg,
        exec.cas(),
        &[],
    );
    GenRule.analyze(&ctx).unwrap().actions[0].clone()
}

/// Run the same action twice; report each run's `cache_hit`.
fn runs(deterministic: bool) -> (bool, bool) {
    let tmp = tempfile::tempdir().unwrap();
    let pkg_dir = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    let exec = LocalExecutor::new(tmp.path().join(".anneal")).unwrap();
    let action = analyze_genrule(&pkg_dir, &exec, deterministic);
    let first = exec.execute(&action).unwrap();
    assert!(
        first.success(),
        "first run failed: {:?}",
        first.failure_output
    );
    let second = exec.execute(&action).unwrap();
    assert!(second.success());
    (first.cache_hit, second.cache_hit)
}

#[test]
fn default_genrule_reruns_never_caches() {
    // No `deterministic` claim: the engine must not assume the command's
    // purity, so identical inputs still execute.
    assert_eq!(runs(false), (false, false));
}

#[test]
fn deterministic_opt_in_earns_caching() {
    assert_eq!(runs(true), (false, true));
}
