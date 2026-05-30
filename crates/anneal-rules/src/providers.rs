//! Providers — the typed values passed along dependency edges (§5.2).
//!
//! Milestone 1 needs one: a [`FileSet`]. A target exposes a [`ProviderSet`]; a
//! dependent reads it through its [`RuleContext`]. Richer providers (ToolchainInfo,
//! TestSuite, LibraryInfo, …) are added as the rules that produce them land.
//!
//! [`RuleContext`]: crate::RuleContext

use std::path::PathBuf;

use anneal_core::Digest;

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
