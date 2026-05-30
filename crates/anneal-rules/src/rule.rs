//! The [`Rule`] trait — the narrow system/rule interface (§5.2, §5.3).

use std::fmt;
use std::path::PathBuf;

use anneal_exec::Action;

use crate::attrs::AttrError;
use crate::context::RuleContext;
use crate::providers::ProviderSet;
use crate::schema::AttrSchema;

/// What a rule produces from one configured target: the actions to run and the
/// providers it exposes to dependents.
#[derive(Debug)]
pub struct Analysis {
    pub actions: Vec<Action>,
    pub providers: ProviderSet,
}

/// A first-party rule. The whole interface is [`Rule::analyze`]: attributes +
/// configuration + dependency providers → actions + providers. Everything about how
/// the resulting actions actually run is the kernel's concern, not the rule's.
pub trait Rule {
    /// The rule's kind as written in a `BUILD` file (e.g. `"genrule"`).
    fn kind(&self) -> &'static str;

    /// The attribute schema, validated at load time (§4.3). The implicit `name`
    /// attribute is handled by the loader and is not listed here.
    fn schema(&self) -> &'static [AttrSchema];

    /// Analyze one configured target.
    fn analyze(&self, ctx: &RuleContext) -> Result<Analysis, RuleError>;
}

/// A failure while analyzing a target.
#[derive(Debug)]
pub enum RuleError {
    /// A missing or wrong-typed attribute.
    Attr(AttrError),
    /// Failed to read a declared source file.
    Source {
        path: PathBuf,
        error: std::io::Error,
    },
    /// A rule-specific validation failure.
    Message(String),
}

impl From<AttrError> for RuleError {
    fn from(e: AttrError) -> Self {
        RuleError::Attr(e)
    }
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuleError::Attr(e) => write!(f, "{e}"),
            RuleError::Source { path, error } => {
                write!(f, "reading source `{}`: {error}", path.display())
            }
            RuleError::Message(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for RuleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RuleError::Attr(e) => Some(e),
            RuleError::Source { error, .. } => Some(error),
            RuleError::Message(_) => None,
        }
    }
}
