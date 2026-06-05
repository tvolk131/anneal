//! The `nickel_eval` rule (§13.1) — evaluate a Nickel file to a chosen data format.
//!
//! Pure: it consumes **no axes** and is **platform-independent**, so its cache key is
//! identical across every configuration (§6.3) — one evaluation is shared workspace-
//! wide. The output is exposed as a provider so a consumer can route it across a
//! language boundary (§14): the Milestone 1 demonstration is `nickel_eval` → a
//! `pnpm_workspace` that imports the generated JSON as an ordinary module (§14.3).
//!
//! **Format (§5.6).** Nickel can export to a fixed *capability* set of formats. The
//! `format` attribute picks one (default `json`), validated against that capability —
//! an unsupported value is a rule-boundary error, not a shell failure. Following the
//! near-term variant idiom (§5.6, one target per variant until demand-driven output
//! pruning exists), each target produces a *single* format as its default output;
//! multiple formats are multiple targets. The named-group menu (one target offering
//! several formats, consumer selects) is the additive future shape.
//!
//! Milestone 1 scope: a single entry `src` (self-contained; Nickel `import`s of other
//! files would need to be declared — deferred). The "generated native package"
//! shaping (wrapping the output with a `package.json`) lives with the routing step.

use std::path::{Path, PathBuf};

use anneal_exec::Action;

use crate::context::RuleContext;
use crate::providers::{Artifact, ArtifactSource, FileSet, ProviderSet};
use crate::rule::{Analysis, Rule, RuleError};
use crate::schema::{AttrSchema, AttrType};

const SCHEMA: &[AttrSchema] = &[
    AttrSchema::required("src", AttrType::String),
    AttrSchema::optional("out", AttrType::String),
    AttrSchema::optional("format", AttrType::String),
];

/// The formats `nickel export` supports — the rule's capability (§5.6). A target's
/// `format` must be one of these.
const CAPABILITY: &[&str] = &["json", "toml", "yaml", "yaml-documents", "text"];

pub struct NickelEval;

impl Rule for NickelEval {
    fn kind(&self) -> &'static str {
        "nickel_eval"
    }

    fn schema(&self) -> &'static [AttrSchema] {
        SCHEMA
    }

    fn analyze(&self, ctx: &RuleContext) -> Result<Analysis, RuleError> {
        let src = ctx.attrs().string("src")?.to_owned();

        // Pick and validate the format against the rule's capability (§5.6).
        let format = ctx
            .attrs()
            .string_opt("format")?
            .unwrap_or("json")
            .to_owned();
        if !CAPABILITY.contains(&format.as_str()) {
            return Err(RuleError::Message(format!(
                "nickel_eval: unsupported format {format:?}; nickel exports {CAPABILITY:?}"
            )));
        }

        let out = ctx
            .attrs()
            .string_opt("out")?
            .map(str::to_owned)
            .unwrap_or_else(|| format!("output.{}", default_extension(&format)));

        let nickel_dir = which_dir("nickel").ok_or_else(|| {
            RuleError::Message("`nickel` not found on PATH; nickel_eval requires Nickel".to_owned())
        })?;

        let src_artifact = ctx.source_artifact(Path::new(&src))?;
        let ArtifactSource::Source(src_digest) = &src_artifact.source else {
            unreachable!("source_artifact yields a Source");
        };

        let action_id = format!("nickel_eval {}", ctx.label());
        let script = format!("nickel export {src} --format {format} > {out}");
        let path_env = format!("{}:/usr/bin:/bin", nickel_dir.to_string_lossy());

        let action = Action::builder(
            action_id.clone(),
            vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
        )
        .input(src.clone(), PathBuf::from(&src), *src_digest)
        .output(out.clone(), PathBuf::from(&out))
        .env("PATH", path_env)
        // Pure data evaluation: no axes, no platform dependence ⇒ configuration-
        // invariant cache key (§6.3).
        .platform_independent()
        .configured(ctx.config().clone(), Vec::new());

        // Expose the produced JSON so a consumer can route it (e.g. into pnpm).
        let providers = ProviderSet {
            files: Some(FileSet {
                files: vec![Artifact {
                    path: PathBuf::from(&out),
                    source: ArtifactSource::Output {
                        action: action_id,
                        name: out,
                    },
                }],
            }),
        };

        Ok(Analysis {
            actions: vec![action.build()],
            providers,
        })
    }
}

fn which_dir(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find(|dir| dir.join(tool).is_file())
}

/// Conventional file extension for the default `out` name of a given format.
fn default_extension(format: &str) -> &'static str {
    match format {
        "toml" => "toml",
        "yaml" | "yaml-documents" => "yaml",
        "text" => "txt",
        _ => "json",
    }
}
