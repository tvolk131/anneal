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
    /// A table whose structure the rule interprets. Values may nest (e.g.
    /// `scripts = { "build": { "kind": "build", "outputs": ["dist"] } }`).
    Dict(BTreeMap<String, AttrValue>),
    /// A table from labels to strings (`data = { "//pkg:t": "dest" }`). The labels are
    /// dependency edges; the strings are per-edge metadata the rule reads.
    LabelKeyedStringDict(Vec<(Label, String)>),
}

impl AttrValue {
    /// The string if this is a [`AttrValue::String`], else `None`.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            AttrValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// The items if this is a [`AttrValue::StringList`], else `None`.
    pub fn as_string_list(&self) -> Option<&[String]> {
        match self {
            AttrValue::StringList(v) => Some(v),
            _ => None,
        }
    }

    /// The entries if this is a [`AttrValue::Dict`], else `None`.
    pub fn as_dict(&self) -> Option<&BTreeMap<String, AttrValue>> {
        match self {
            AttrValue::Dict(m) => Some(m),
            _ => None,
        }
    }
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

    /// An optional string attribute: `None` when absent, error when present with the
    /// wrong type.
    pub fn string_opt(&self, name: &str) -> Result<Option<&str>, AttrError> {
        match self.map.get(name) {
            None => Ok(None),
            Some(AttrValue::String(s)) => Ok(Some(s)),
            Some(_) => Err(AttrError::wrong_type(name, "string")),
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

    /// An optional dict attribute: `None` when absent, error when present with the
    /// wrong type. The rule interprets the table's structure itself.
    pub fn dict_opt(&self, name: &str) -> Result<Option<&BTreeMap<String, AttrValue>>, AttrError> {
        match self.map.get(name) {
            None => Ok(None),
            Some(AttrValue::Dict(m)) => Ok(Some(m)),
            Some(_) => Err(AttrError::wrong_type(name, "dict")),
        }
    }

    /// An optional label-keyed-string-dict attribute: empty when absent, error when
    /// present with the wrong type. Returns `(label, string)` pairs in declaration order.
    pub fn label_keyed_strings_opt(&self, name: &str) -> Result<&[(Label, String)], AttrError> {
        match self.map.get(name) {
            None => Ok(&[]),
            Some(AttrValue::LabelKeyedStringDict(pairs)) => Ok(pairs),
            Some(_) => Err(AttrError::wrong_type(name, "label-keyed string dict")),
        }
    }
}

/// Builder for [`Attrs`].
pub struct AttrsBuilder {
    map: BTreeMap<String, AttrValue>,
}

impl AttrsBuilder {
    pub fn string(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.map
            .insert(name.into(), AttrValue::String(value.into()));
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

    /// Insert an already-typed value (used by the loader after schema coercion).
    pub fn value(mut self, name: impl Into<String>, value: AttrValue) -> Self {
        self.map.insert(name.into(), value);
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
    WrongType {
        name: String,
        expected: &'static str,
    },
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
