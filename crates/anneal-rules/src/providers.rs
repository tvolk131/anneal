//! Providers — the typed values passed along dependency edges (§5.2).
//!
//! Milestone 1 needs one: a [`FileSet`]. A target exposes a [`ProviderSet`]; a
//! dependent reads it through its [`RuleContext`]. Richer providers (ToolchainInfo,
//! TestSuite, LibraryInfo, …) are added as the rules that produce them land.
//!
//! [`RuleContext`]: crate::RuleContext

use std::path::PathBuf;

use anneal_core::Digest;

/// A content-addressed file with a logical (relative) path. Used for source files
/// and `filegroup` outputs, whose digests are known at analysis time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub path: PathBuf,
    pub digest: Digest,
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
