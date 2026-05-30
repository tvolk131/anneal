//! Analysis errors.

use std::fmt;

use anneal_core::Label;
use anneal_rules::RuleError;

/// A failure while analyzing a target graph.
#[derive(Debug)]
pub enum AnalysisError {
    /// A referenced target is not present in the graph.
    MissingTarget(Label),
    /// A dependency cycle was detected reaching this label.
    Cycle(Label),
    /// The target's rule kind has no registered implementation.
    UnknownRule { label: Label, kind: String },
    /// The rule's `analyze` failed.
    Rule { label: Label, error: RuleError },
}

impl fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnalysisError::MissingTarget(label) => {
                write!(f, "no such target `{label}` in the graph")
            }
            AnalysisError::Cycle(label) => {
                write!(f, "dependency cycle detected at `{label}`")
            }
            AnalysisError::UnknownRule { label, kind } => {
                write!(f, "`{label}`: unknown rule kind `{kind}`")
            }
            AnalysisError::Rule { label, error } => {
                write!(f, "`{label}`: {error}")
            }
        }
    }
}

impl std::error::Error for AnalysisError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AnalysisError::Rule { error, .. } => Some(error),
            _ => None,
        }
    }
}
