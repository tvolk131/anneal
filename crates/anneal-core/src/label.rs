//! Target labels — the §19.4 grammar, restricted to what Milestone 1 uses.
//!
//! Supported here: absolute labels `//package/path` and `//package/path:target`.
//! Deferred to later layers: repo prefixes (`@repo//…`, reserved, v1 default
//! workspace only), package-relative `:target` (needs a base package for context),
//! and glob forms (`//crates/...`, `//pkg:*`) which are a *query* concept, not a
//! concrete target — they are rejected here so a `Label` always names exactly one
//! target.

use std::fmt;

/// A fully-qualified, canonical reference to exactly one target.
///
/// Fields are private and the only constructor validates against the grammar, so a
/// `Label` is always well-formed and always prints in canonical `//package:target`
/// form.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Label {
    /// Package path with no leading `//` and no trailing `:target` (may be empty
    /// for the root package).
    package: String,
    /// Target name within the package.
    target: String,
}

impl Label {
    /// Parse an absolute label. The target defaults to the last package segment
    /// when omitted (`//crates/my_lib` ⇒ `//crates/my_lib:my_lib`).
    pub fn parse(s: &str) -> Result<Label, LabelParseError> {
        if s.starts_with('@') {
            return Err(LabelParseError::RepoUnsupported);
        }
        let rest = s.strip_prefix("//").ok_or(LabelParseError::NotAbsolute)?;
        if rest.contains('*') || rest.split('/').any(|seg| seg == "...") {
            return Err(LabelParseError::Glob);
        }

        let (package, explicit_target) = match rest.split_once(':') {
            Some((p, t)) => (p, Some(t)),
            None => (rest, None),
        };

        if !package.is_empty() {
            for seg in package.split('/') {
                if !is_identifier(seg) {
                    return Err(LabelParseError::BadPackage(package.to_owned()));
                }
            }
        }

        let target = match explicit_target {
            Some(t) => t.to_owned(),
            None => package
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .ok_or(LabelParseError::MissingTarget)?
                .to_owned(),
        };

        if !is_target_name(&target) {
            return Err(LabelParseError::BadTarget(target));
        }

        Ok(Label {
            package: package.to_owned(),
            target,
        })
    }

    /// The package path (no leading `//`, no `:target`).
    pub fn package(&self) -> &str {
        &self.package
    }

    /// The target name within the package.
    pub fn target(&self) -> &str {
        &self.target
    }
}

/// `identifier = [a-zA-Z0-9_][a-zA-Z0-9_\-]*`
fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `target_name = [a-zA-Z0-9_][a-zA-Z0-9_\-.]*`
fn is_target_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "//{}:{}", self.package, self.target)
    }
}

impl fmt::Debug for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

/// Failure parsing a [`Label`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelParseError {
    /// `@repo//…` repo prefixes are reserved but unsupported in v1.
    RepoUnsupported,
    /// Label did not start with `//` (relative labels need a base package).
    NotAbsolute,
    /// Glob forms (`...`, `*`) are a query concept, not a concrete target.
    Glob,
    /// A package path segment violated the identifier grammar.
    BadPackage(String),
    /// The target name violated the grammar.
    BadTarget(String),
    /// `//` with an empty package and no explicit target.
    MissingTarget,
}

impl fmt::Display for LabelParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LabelParseError::RepoUnsupported => {
                f.write_str("repo prefixes (`@repo//…`) are reserved but unsupported in v1")
            }
            LabelParseError::NotAbsolute => {
                f.write_str("label must be absolute (start with `//`)")
            }
            LabelParseError::Glob => {
                f.write_str("glob labels (`...`, `*`) are not a concrete target")
            }
            LabelParseError::BadPackage(p) => write!(f, "invalid package path {p:?}"),
            LabelParseError::BadTarget(t) => write!(f, "invalid target name {t:?}"),
            LabelParseError::MissingTarget => {
                f.write_str("label `//` needs an explicit `:target`")
            }
        }
    }
}

impl std::error::Error for LabelParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Label {
        Label::parse(s).unwrap()
    }

    #[test]
    fn explicit_target() {
        let l = parse("//crates/anneal_core:anneal_core");
        assert_eq!(l.package(), "crates/anneal_core");
        assert_eq!(l.target(), "anneal_core");
        assert_eq!(l.to_string(), "//crates/anneal_core:anneal_core");
    }

    #[test]
    fn implied_target_is_last_segment() {
        let l = parse("//crates/my_lib");
        assert_eq!(l.target(), "my_lib");
        assert_eq!(l.to_string(), "//crates/my_lib:my_lib");
    }

    #[test]
    fn root_package_with_explicit_target() {
        let l = parse("//:tool");
        assert_eq!(l.package(), "");
        assert_eq!(l.target(), "tool");
        assert_eq!(l.to_string(), "//:tool");
    }

    #[test]
    fn dotted_target_name_allowed() {
        assert_eq!(parse("//pkg:a.b.c").target(), "a.b.c");
    }

    #[test]
    fn rejects_bad_forms() {
        assert_eq!(Label::parse("@repo//pkg"), Err(LabelParseError::RepoUnsupported));
        assert_eq!(Label::parse("pkg:t"), Err(LabelParseError::NotAbsolute));
        assert_eq!(Label::parse("//crates/..."), Err(LabelParseError::Glob));
        assert_eq!(Label::parse("//pkg:*"), Err(LabelParseError::Glob));
        assert_eq!(Label::parse("//"), Err(LabelParseError::MissingTarget));
        assert!(matches!(
            Label::parse("//bad pkg:t"),
            Err(LabelParseError::BadPackage(_))
        ));
        assert!(matches!(
            Label::parse("//pkg:-bad"),
            Err(LabelParseError::BadTarget(_))
        ));
    }
}
