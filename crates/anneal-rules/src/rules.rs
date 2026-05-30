//! The Milestone 1 first-party rules and their registry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anneal_exec::Action;

use crate::context::RuleContext;
use crate::providers::{Artifact, ArtifactSource, FileSet, ProviderSet};
use crate::rule::{Analysis, Rule, RuleError};
use crate::schema::{AttrSchema, AttrType};

const FILEGROUP_SCHEMA: &[AttrSchema] = &[AttrSchema::required("srcs", AttrType::StringList)];
const ALIAS_SCHEMA: &[AttrSchema] = &[AttrSchema::required("actual", AttrType::Label)];
const GENRULE_SCHEMA: &[AttrSchema] = &[
    AttrSchema::optional("srcs", AttrType::StringList),
    AttrSchema::optional("deps", AttrType::LabelList),
    AttrSchema::required("outs", AttrType::StringList),
    AttrSchema::required("cmd", AttrType::String),
];

/// `filegroup(name, srcs)` — groups source files into a target. No actions; exposes
/// a [`FileSet`] of the resolved sources.
pub struct FileGroup;

impl Rule for FileGroup {
    fn kind(&self) -> &'static str {
        "filegroup"
    }

    fn schema(&self) -> &'static [AttrSchema] {
        FILEGROUP_SCHEMA
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

    fn schema(&self) -> &'static [AttrSchema] {
        ALIAS_SCHEMA
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

    fn schema(&self) -> &'static [AttrSchema] {
        GENRULE_SCHEMA
    }

    fn analyze(&self, ctx: &RuleContext) -> Result<Analysis, RuleError> {
        let direct_srcs = ctx.attrs().string_list_opt("srcs")?;
        let outs = ctx.attrs().string_list("outs")?;
        let cmd = ctx.attrs().string("cmd")?;

        if outs.is_empty() {
            return Err(RuleError::Message(
                "genrule `outs` must declare at least one output".to_owned(),
            ));
        }

        // Inputs = direct source files + every file provided by `deps` targets. A
        // dependency artifact may be a resolved source (`filegroup`) or another
        // action's produced output (`genrule`); either flows straight through as the
        // matching input source.
        let mut inputs: Vec<Artifact> = Vec::new();
        for src in direct_srcs {
            inputs.push(ctx.source_artifact(Path::new(src))?);
        }
        for dep in ctx.deps() {
            if let Some(file_set) = &dep.providers.files {
                inputs.extend(file_set.files.iter().cloned());
            }
        }

        // `$(SRCS)` expands to every input path; `$(OUTS)` to every output path.
        let srcs_joined = inputs
            .iter()
            .map(|a| a.path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let expanded = cmd
            .replace("$(SRCS)", &srcs_joined)
            .replace("$(OUTS)", &outs.join(" "));

        // The action's name doubles as its graph id; outputs are referenced as
        // `(action_id, output_name)` by any consumer.
        let action_id = format!("genrule {}", ctx.label());
        let command = vec!["/bin/sh".to_owned(), "-c".to_owned(), expanded];
        let mut builder = Action::builder(action_id.clone(), command);
        for artifact in &inputs {
            let name = artifact.path.to_string_lossy().into_owned();
            match &artifact.source {
                ArtifactSource::Source(digest) => {
                    builder = builder.input(name, artifact.path.clone(), *digest);
                }
                ArtifactSource::Output {
                    action: producer,
                    name: output,
                } => {
                    builder =
                        builder.input_from_output(name, artifact.path.clone(), producer, output);
                }
            }
        }
        for out in outs {
            builder = builder.output(out.clone(), PathBuf::from(out));
        }
        // genrule is configuration-invariant for now; per-rule axis mapping (§13.6)
        // is a later increment.
        builder = builder.configured(ctx.config().clone(), Vec::new());

        // Expose the produced outputs so dependents can consume them — the digests
        // are unknown until execution, so they are Output references resolved then.
        let provided_outputs = outs
            .iter()
            .map(|out| Artifact {
                path: PathBuf::from(out),
                source: ArtifactSource::Output {
                    action: action_id.clone(),
                    name: out.clone(),
                },
            })
            .collect();

        Ok(Analysis {
            actions: vec![builder.build()],
            providers: ProviderSet {
                files: Some(FileSet {
                    files: provided_outputs,
                }),
            },
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
