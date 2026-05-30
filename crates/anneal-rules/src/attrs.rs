//! Typed rule attributes.
//!
//! A rule reads its attributes through typed accessors that return a structured
//! [`AttrError`] on a missing or wrong-typed value (§4.3 schema validation at the
//! rule boundary). The loader will build [`Attrs`] from Starlark values; until then
//! tests build them with [`Attrs::builder`].

use std::collections::BTreeMap;
use std::fmt;

use anneal_core::Label;

/// A single attribute value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrValue {
    String(String),
    Int(i64),
    Bool(bool),
    StringList(Vec<String>),
    Label(Label),
    LabelList(Vec<Label>),
}

/// A target's attributes, keyed by name.
#[derive(Debug, Clone, Default)]
pub struct Attrs {
    map: BTreeMap<String, AttrValue>,
}

impl Attrs {
    pub fn builder() -> AttrsBuilder {
        AttrsBuilder {
            map: BTreeMap::new(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&AttrValue> {
        self.map.get(name)
    }

    /// A required string attribute.
    pub fn string(&self, name: &str) -> Result<&str, AttrError> {
        match self.map.get(name) {
            Some(AttrValue::String(s)) => Ok(s),
            Some(_) => Err(AttrError::wrong_type(name, "string")),
            None => Err(AttrError::Missing(name.to_owned())),
        }
    }

    /// A required list-of-strings attribute.
    pub fn string_list(&self, name: &str) -> Result<&[String], AttrError> {
        match self.map.get(name) {
            Some(AttrValue::StringList(v)) => Ok(v),
            Some(_) => Err(AttrError::wrong_type(name, "string list")),
            None => Err(AttrError::Missing(name.to_owned())),
        }
    }

    /// An optional list-of-strings attribute: empty when absent, error when present
    /// with the wrong type.
    pub fn string_list_opt(&self, name: &str) -> Result<&[String], AttrError> {
        match self.map.get(name) {
            None => Ok(&[]),
            Some(AttrValue::StringList(v)) => Ok(v),
            Some(_) => Err(AttrError::wrong_type(name, "string list")),
        }
    }

    /// A required label attribute.
    pub fn label(&self, name: &str) -> Result<&Label, AttrError> {
        match self.map.get(name) {
            Some(AttrValue::Label(l)) => Ok(l),
            Some(_) => Err(AttrError::wrong_type(name, "label")),
            None => Err(AttrError::Missing(name.to_owned())),
        }
    }
}

/// Builder for [`Attrs`].
pub struct AttrsBuilder {
    map: BTreeMap<String, AttrValue>,
}

impl AttrsBuilder {
    pub fn string(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.map.insert(name.into(), AttrValue::String(value.into()));
        self
    }

    pub fn strings(
        mut self,
        name: impl Into<String>,
        values: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let list = values.into_iter().map(Into::into).collect();
        self.map.insert(name.into(), AttrValue::StringList(list));
        self
    }

    pub fn label(mut self, name: impl Into<String>, value: Label) -> Self {
        self.map.insert(name.into(), AttrValue::Label(value));
        self
    }

    pub fn build(self) -> Attrs {
        Attrs { map: self.map }
    }
}

/// A missing or wrong-typed attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrError {
    Missing(String),
    WrongType { name: String, expected: &'static str },
}

impl AttrError {
    fn wrong_type(name: &str, expected: &'static str) -> Self {
        AttrError::WrongType {
            name: name.to_owned(),
            expected,
        }
    }
}

impl fmt::Display for AttrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttrError::Missing(name) => write!(f, "missing required attribute `{name}`"),
            AttrError::WrongType { name, expected } => {
                write!(f, "attribute `{name}` must be a {expected}")
            }
        }
    }
}

impl std::error::Error for AttrError {}
