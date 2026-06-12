//! The v3 analysis-surface increment, end-to-end at the context level:
//! `declare_state` semantics (rule-kind scoping, cross-target idempotence,
//! mismatch as a hard error) and analysis-time tool queries.

use std::path::PathBuf;

use anneal_core::{AxisValues, Configuration, Label, Platform};
use anneal_exec::{LocalExecutor, QuerySpec, Toolchain};
use anneal_rules::{
    Attestation, Attrs, Concurrency, PersistentStateDecl, RuleContext, StateKind, StateRegistry,
};

// Minimal shell/runtime helpers (the anneal-exec test-support module is not
// shareable across crates).
fn shell_path() -> PathBuf {
    ["/bin/sh", "/usr/bin/sh", "/bin/bash"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .expect("test host has a POSIX shell")
}

fn shell_argv(script: &str) -> Vec<String> {
    vec![
        shell_path().to_string_lossy().into_owned(),
        "-c".to_owned(),
        script.to_owned(),
    ]
}

fn system_bin_dirs() -> Vec<PathBuf> {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .collect()
}

fn system_runtime() -> Toolchain {
    let roots = ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/usr/lib", "/usr/lib64"]
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();
    Toolchain::new(
        "test-system-runtime",
        format!("shell={}", shell_path().display()),
        system_bin_dirs(),
        roots,
    )
    .unwrap()
}

fn system_path_env() -> String {
    system_bin_dirs()
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":")
}

fn decl(epoch: u32) -> PersistentStateDecl {
    PersistentStateDecl {
        namespace: "test-state",
        shard: vec!["shard-a".into()],
        kind: StateKind::Interleaved {
            concurrency: Concurrency::Exclusive,
            attestation: Attestation {
                epoch,
                rationale: "test",
            },
        },
        paths: vec![PathBuf::from("state")],
    }
}

struct Fixture {
    attrs: Attrs,
    config: Configuration,
    package_dir: PathBuf,
    exec: LocalExecutor,
}

impl Fixture {
    fn new() -> (Self, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let fx = Fixture {
            attrs: Attrs::default(),
            config: Configuration::new(Platform::new("host", "host"), AxisValues::default()),
            package_dir: dir.path().to_path_buf(),
            exec: LocalExecutor::new(dir.path().join(".anneal")).unwrap(),
        };
        (fx, dir)
    }

    fn ctx<'a>(&'a self, registry: &'a StateRegistry) -> RuleContext<'a> {
        RuleContext::new(
            Label::parse("//pkg:t").unwrap(),
            &self.attrs,
            &self.config,
            &self.package_dir,
            self.exec.cas(),
            &[],
        )
        .with_rule_kind("test_rule")
        .with_state_registry(registry)
        .with_executor(&self.exec)
    }
}

#[test]
fn declare_state_is_idempotent_and_mismatch_is_a_hard_error() {
    let (fx, _dir) = Fixture::new();
    let registry = StateRegistry::new();
    let ctx = fx.ctx(&registry);

    // Bit-identical declarations across "targets": same handle key.
    let a = ctx.declare_state(decl(1)).unwrap();
    let b = ctx.declare_state(decl(1)).unwrap();
    assert_eq!(a.key(), b.key());

    // Same identity, different attestation epoch: a fork of the trust
    // contract — hard error, never silently resolved.
    assert!(ctx.declare_state(decl(2)).is_err());
}

#[test]
fn declare_state_requires_rule_scope() {
    let (fx, _dir) = Fixture::new();
    let ctx = RuleContext::new(
        Label::parse("//pkg:t").unwrap(),
        &fx.attrs,
        &fx.config,
        &fx.package_dir,
        fx.exec.cas(),
        &[],
    );
    assert!(
        ctx.declare_state(decl(1)).is_err(),
        "state keys are rule-scoped (§2.6); no scope, no state"
    );
}

#[test]
fn analysis_time_query_runs_and_caches() {
    let (fx, _dir) = Fixture::new();
    let registry = StateRegistry::new();
    let ctx = fx.ctx(&registry);

    let spec = QuerySpec::builder("ctx-query", shell_argv("echo queried"))
        .toolchain(system_runtime())
        .env("PATH", system_path_env())
        .build()
        .unwrap();

    let first = ctx.query(&spec).unwrap();
    assert_eq!(String::from_utf8_lossy(&first).trim(), "queried");
    let second = ctx.query(&spec).unwrap();
    assert_eq!(first, second);
}

#[test]
fn query_without_executor_fails_loudly() {
    let (fx, _dir) = Fixture::new();
    let ctx = RuleContext::new(
        Label::parse("//pkg:t").unwrap(),
        &fx.attrs,
        &fx.config,
        &fx.package_dir,
        fx.exec.cas(),
        &[],
    );
    let spec = QuerySpec::builder("ctx-query", shell_argv("echo hi"))
        .toolchain(system_runtime())
        .env("PATH", system_path_env())
        .build()
        .unwrap();
    assert!(ctx.query(&spec).is_err());
}
