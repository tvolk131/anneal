//! Analysis errors.

use std::fmt;
use std::path::PathBuf;

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
    /// Two generated outputs claim the same workspace-relative path.
    GeneratedOutputCollision {
        path: PathBuf,
        first_label: Label,
        first_action: String,
        first_output: String,
        second_label: Label,
        second_action: String,
        second_output: String,
    },
    /// A generated output claims a workspace-relative path already occupied by a source.
    GeneratedOutputShadowsSource {
        path: PathBuf,
        label: Label,
        action: String,
        output: String,
    },
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
            AnalysisError::GeneratedOutputCollision {
                path,
                first_label,
                first_action,
                first_output,
                second_label,
                second_action,
                second_output,
            } => {
                write!(
                    f,
                    "generated output path `{}` is declared by both `{}` output `{}` on `{}` and `{}` output `{}` on `{}`",
                    path.display(),
                    first_action,
                    first_output,
                    first_label,
                    second_action,
                    second_output,
                    second_label
                )
            }
            AnalysisError::GeneratedOutputShadowsSource {
                path,
                label,
                action,
                output,
            } => {
                write!(
                    f,
                    "generated output path `{}` from `{}` output `{}` on `{}` shadows a source file",
                    path.display(),
                    action,
                    output,
                    label
                )
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
