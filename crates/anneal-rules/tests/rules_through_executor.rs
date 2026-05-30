//! Phase 2 vertical slice: a rule's analysis produces real [`Action`]s that run and
//! cache through the Phase 1 kernel.
//!
//! [`Action`]: anneal_exec::Action

use anneal_core::{AxisValues, Configuration, Platform};
use anneal_exec::{Executor, LocalExecutor};
use anneal_rules::{Attrs, GenRule, ProviderSet, ResolvedDep, Rule, RuleContext};
use anneal_rules::{Alias, FileGroup};

fn host_config() -> Configuration {
    Configuration::new(Platform::new("host", "host"), AxisValues::default())
}

/// A workspace on disk: a package directory plus a `LocalExecutor` sharing one CAS.
struct Fixture {
    _tmp: tempfile::TempDir,
    package_dir: std::path::PathBuf,
    exec: LocalExecutor,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let package_dir = tmp.path().join("pkg");
        std::fs::create_dir_all(&package_dir).unwrap();
        let exec = LocalExecutor::new(tmp.path().join(".mybuild")).unwrap();
        Fixture {
            _tmp: tmp,
            package_dir,
            exec,
        }
    }

    fn write_source(&self, name: &str, contents: &str) {
        std::fs::write(self.package_dir.join(name), contents).unwrap();
    }
}

#[test]
fn genrule_analyzes_executes_and_caches() {
    let fx = Fixture::new();
    fx.write_source("a.txt", "alpha\n");
    fx.write_source("b.txt", "beta\n");

    let config = host_config();
    let attrs = Attrs::builder()
        .strings("srcs", ["a.txt", "b.txt"])
        .strings("outs", ["combined.txt"])
        .string("cmd", "cat $(SRCS) > $(OUTS)")
        .build();
    let label = anneal_core::Label::parse("//pkg:combined").unwrap();
    let ctx = RuleContext::new(
        label,
        &attrs,
        &config,
        &fx.package_dir,
        fx.exec.cas(),
        &[],
    );

    // Analyze: the rule emits exactly one action.
    let analysis = GenRule.analyze(&ctx).unwrap();
    assert_eq!(analysis.actions.len(), 1);

    // Execute the emitted action through the kernel: cache miss, real run.
    let action = &analysis.actions[0];
    let first = fx.exec.execute(action).unwrap();
    assert!(first.success() && !first.cache_hit);
    let out = fx
        .exec
        .cas()
        .get(first.outputs.get("combined.txt").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "alpha\nbeta\n");

    // Re-run the identical action: cache hit, no re-execution.
    let second = fx.exec.execute(action).unwrap();
    assert!(second.cache_hit);
    assert_eq!(second.outputs, first.outputs);
}

#[test]
fn filegroup_provides_resolved_sources() {
    let fx = Fixture::new();
    fx.write_source("x.txt", "x");
    fx.write_source("y.txt", "y");

    let config = host_config();
    let attrs = Attrs::builder().strings("srcs", ["x.txt", "y.txt"]).build();
    let label = anneal_core::Label::parse("//pkg:group").unwrap();
    let ctx = RuleContext::new(label, &attrs, &config, &fx.package_dir, fx.exec.cas(), &[]);

    let analysis = FileGroup.analyze(&ctx).unwrap();
    assert!(analysis.actions.is_empty());
    let files = analysis.providers.files.expect("filegroup exposes a FileSet");
    assert_eq!(files.files.len(), 2);
    // Sources are resolved content addresses.
    assert_eq!(
        files.files[0].source,
        anneal_rules::ArtifactSource::Source(anneal_core::Digest::of(b"x"))
    );
}

#[test]
fn alias_forwards_dependency_providers() {
    let fx = Fixture::new();
    fx.write_source("z.txt", "z");

    let config = host_config();

    // First analyze a filegroup, then feed its providers to an alias as a dep.
    let fg_attrs = Attrs::builder().strings("srcs", ["z.txt"]).build();
    let fg_label = anneal_core::Label::parse("//pkg:group").unwrap();
    let fg_ctx = RuleContext::new(fg_label.clone(), &fg_attrs, &config, &fx.package_dir, fx.exec.cas(), &[]);
    let fg = FileGroup.analyze(&fg_ctx).unwrap();

    let deps = [ResolvedDep {
        label: fg_label,
        providers: fg.providers.clone(),
    }];
    let alias_attrs = Attrs::builder()
        .label("actual", anneal_core::Label::parse("//pkg:group").unwrap())
        .build();
    let alias_label = anneal_core::Label::parse("//pkg:g").unwrap();
    let alias_ctx = RuleContext::new(alias_label, &alias_attrs, &config, &fx.package_dir, fx.exec.cas(), &deps);

    let analysis = Alias.analyze(&alias_ctx).unwrap();
    assert_eq!(analysis.providers, fg.providers, "alias forwards the dep's providers");
}

#[test]
fn genrule_without_outs_is_an_error() {
    let fx = Fixture::new();
    let config = host_config();
    let attrs = Attrs::builder().string("cmd", "true").build();
    let label = anneal_core::Label::parse("//pkg:bad").unwrap();
    let ctx = RuleContext::new(label, &attrs, &config, &fx.package_dir, fx.exec.cas(), &[]);

    let err = GenRule.analyze(&ctx).unwrap_err();
    // `outs` is required; the error should name it.
    assert!(
        format!("{err}").contains("outs"),
        "expected an `outs` error, got: {err}"
    );
    assert!(matches!(
        ProviderSet::default(),
        ProviderSet { files: None }
    ));
}
