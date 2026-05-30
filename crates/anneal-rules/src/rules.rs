//! The Milestone 1 first-party rules and their registry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anneal_exec::Action;

use crate::context::RuleContext;
use crate::providers::{FileSet, ProviderSet};
use crate::rule::{Analysis, Rule, RuleError};

/// `filegroup(name, srcs)` — groups source files into a target. No actions; exposes
/// a [`FileSet`] of the resolved sources.
pub struct FileGroup;

impl Rule for FileGroup {
    fn kind(&self) -> &'static str {
        "filegroup"
    }

    fn analyze(&self, ctx: &RuleContext) -> Result<Analysis, RuleError> {
        let srcs = ctx.attrs().string_list("srcs")?;
        let mut files = Vec::with_capacity(srcs.len());
        for src in srcs {
            files.push(ctx.source_artifact(Path::new(src))?);
        }
        Ok(Analysis {
            actions: Vec::new(),
            providers: ProviderSet {
                files: Some(FileSet { files }),
            },
        })
    }
}

/// `alias(name, actual)` — an alternate name for another target. No actions; forwards
/// the providers of its single dependency.
pub struct Alias;

impl Rule for Alias {
    fn kind(&self) -> &'static str {
        "alias"
    }

    fn analyze(&self, ctx: &RuleContext) -> Result<Analysis, RuleError> {
        let dep = ctx.deps().first().ok_or_else(|| {
            RuleError::Message("alias requires its `actual` target to be resolved".to_owned())
        })?;
        Ok(Analysis {
            actions: Vec::new(),
            providers: dep.providers.clone(),
        })
    }
}

/// `genrule(name, srcs, outs, cmd)` — the generic "run a command, produce outputs"
/// escape hatch. Emits one [`Action`]: `/bin/sh -c <cmd>` with `srcs` materialized as
/// inputs and `outs` captured as outputs. `cmd` may use `$(SRCS)` and `$(OUTS)`,
/// which expand to the space-joined input and output paths.
pub struct GenRule;

impl Rule for GenRule {
    fn kind(&self) -> &'static str {
        "genrule"
    }

    fn analyze(&self, ctx: &RuleContext) -> Result<Analysis, RuleError> {
        let srcs = ctx.attrs().string_list_opt("srcs")?;
        let outs = ctx.attrs().string_list("outs")?;
        let cmd = ctx.attrs().string("cmd")?;

        if outs.is_empty() {
            return Err(RuleError::Message(
                "genrule `outs` must declare at least one output".to_owned(),
            ));
        }

        // Resolve sources into content-addressed inputs.
        let mut src_artifacts = Vec::with_capacity(srcs.len());
        for src in srcs {
            src_artifacts.push(ctx.source_artifact(Path::new(src))?);
        }

        // Expand the `$(SRCS)` / `$(OUTS)` make-style variables.
        let expanded = cmd
            .replace("$(SRCS)", &srcs.join(" "))
            .replace("$(OUTS)", &outs.join(" "));

        let command = vec!["/bin/sh".to_owned(), "-c".to_owned(), expanded];
        let mut builder = Action::builder(format!("genrule {}", ctx.label()), command);

        for artifact in &src_artifacts {
            let name = artifact.path.to_string_lossy().into_owned();
            builder = builder.input(name, artifact.path.clone(), artifact.digest);
        }
        for out in outs {
            builder = builder.output(out.clone(), PathBuf::from(out));
        }
        // genrule is configuration-invariant for now; per-rule axis mapping (§13.6)
        // is a later increment.
        builder = builder.configured(ctx.config().clone(), Vec::new());

        Ok(Analysis {
            actions: vec![builder.build()],
            // Exposing genrule's *produced* outputs as a provider requires
            // post-execution digests (the action-graph increment); none for now.
            providers: ProviderSet::default(),
        })
    }
}

/// A registry of rule kinds → implementations, looked up by the loader/analysis.
pub struct RuleRegistry {
    rules: BTreeMap<&'static str, Box<dyn Rule>>,
}

impl RuleRegistry {
    /// Look up a rule by kind, as written in a `BUILD` file.
    pub fn get(&self, kind: &str) -> Option<&dyn Rule> {
        self.rules.get(kind).map(Box::as_ref)
    }

    /// All registered rule kinds.
    pub fn kinds(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.rules.keys().copied()
    }
}

/// The Milestone 1 first-party rule set.
pub fn builtin_rules() -> RuleRegistry {
    let mut rules: BTreeMap<&'static str, Box<dyn Rule>> = BTreeMap::new();
    rules.insert("genrule", Box::new(GenRule));
    rules.insert("filegroup", Box::new(FileGroup));
    rules.insert("alias", Box::new(Alias));
    RuleRegistry { rules }
}
