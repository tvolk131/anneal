//! Providers — the typed values passed along dependency edges (§5.2).
//!
//! Milestone 1 needs one: a [`FileSet`]. A target exposes a [`ProviderSet`]; a
//! dependent reads it through its [`RuleContext`]. Richer providers (ToolchainInfo,
//! TestSuite, LibraryInfo, …) are added as the rules that produce them land.
//!
//! [`RuleContext`]: crate::RuleContext

use std::path::PathBuf;

use anneal_core::Digest;
use anneal_exec::ActionBuilder;

/// Where an artifact's content comes from — mirroring [`anneal_exec::InputSource`],
/// so a provider can carry both resolved sources and not-yet-produced outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactSource {
    /// Concrete content, known at analysis time (a source file, a `filegroup`).
    Source(Digest),
    /// An output produced by an action, identified by the producing action's id
    /// (its [`anneal_exec::Action::name`]) and output name. Resolved to content at
    /// execution time.
    Output { action: String, name: String },
}

/// A file exposed by a target, with a logical (relative) path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub path: PathBuf,
    pub source: ArtifactSource,
}

/// A set of files exposed by a target.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileSet {
    pub files: Vec<Artifact>,
}

/// The typed providers a target exposes to its dependents.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderSet {
    /// The files this target makes available (e.g. a `filegroup`'s sources).
    pub files: Option<FileSet>,
}

/// Wire a routed, dev-tree-visible **data** collection in as action inputs — the one
/// dispatch shared by every rule that consumes generated data (`genrule`,
/// `cargo_workspace`'s `data`, `pnpm_workspace`'s routed edges). Source members are plain
/// inputs (`materialize` skips them — they already live in the tree); produced members are
/// `mirror_to_tree`-routed, so the analyzer's derived view (and `anneal materialize`) parks
/// them for native tools. Each artifact's `path` is both its input name and its tree-relative
/// destination.
///
/// This is *only* for routed-data collections. It is deliberately **not** the path for
/// Source-only collections (`with_sources`) or for produced edges that must stay out of the
/// dev tree (`cargo_workspace`'s fetched `.crate` blobs via `with_crates`, the test-bin
/// compile→run handoff) — those call [`ActionBuilder::input`] / [`ActionBuilder::input_from_output`]
/// directly. Centralizing the dispatch here keeps the "produced ⇒ routed" rule a single source
/// of truth, so a new rule can't silently wire a generated input as non-routed and break
/// `materialize`.
pub(crate) fn route_data_inputs(
    mut builder: ActionBuilder,
    artifacts: &[Artifact],
) -> ActionBuilder {
    for artifact in artifacts {
        let name = artifact.path.to_string_lossy().into_owned();
        match &artifact.source {
            ArtifactSource::Source(digest) => {
                builder = builder.input(name, artifact.path.clone(), *digest);
            }
            ArtifactSource::Output {
                action,
                name: output,
            } => {
                builder =
                    builder.routed_input_from_output(name, artifact.path.clone(), action, output);
            }
        }
    }
    builder
}
