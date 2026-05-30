//! The `nickel_eval` rule (§13.1) — evaluate a Nickel file to JSON.
//!
//! Pure: it consumes **no axes** and is **platform-independent**, so its cache key is
//! identical across every configuration (§6.3) — one evaluation is shared workspace-
//! wide. The JSON output is exposed as a provider so a consumer can route it across a
//! language boundary (§14): the Milestone 1 demonstration is `nickel_eval` → a
//! `pnpm_workspace` that imports the generated JSON as an ordinary module (§14.3).
//!
//! Milestone 1 scope: a single entry `src` (self-contained; Nickel `import`s of other
//! files would need to be declared — deferred). The "generated native package"
//! shaping (wrapping the JSON with a `package.json`) lives with the routing step.

use std::path::{Path, PathBuf};

use anneal_exec::Action;

use crate::context::RuleContext;
use crate::providers::{Artifact, ArtifactSource, FileSet, ProviderSet};
use crate::rule::{Analysis, Rule, RuleError};
use crate::schema::{AttrSchema, AttrType};

const SCHEMA: &[AttrSchema] = &[
    AttrSchema::required("src", AttrType::String),
    AttrSchema::optional("out", AttrType::String),
];

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
        let out = ctx
            .attrs()
            .string_opt("out")?
            .unwrap_or("output.json")
            .to_owned();

        let nickel_dir = which_dir("nickel").ok_or_else(|| {
            RuleError::Message("`nickel` not found on PATH; nickel_eval requires Nickel".to_owned())
        })?;

        let src_artifact = ctx.source_artifact(Path::new(&src))?;
        let ArtifactSource::Source(src_digest) = &src_artifact.source else {
            unreachable!("source_artifact yields a Source");
        };

        let action_id = format!("nickel_eval {}", ctx.label());
        let script = format!("nickel export {src} --format json > {out}");
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
