//! Attribute schemas — what the loader validates rule arguments against at load
//! time (§4.3). A rule *declares* its schema; the loader (`anneal-loader`) consumes
//! it to validate and coerce the Starlark values written in a `BUILD` file. Keeping
//! the schema with the rule is what stops the loader and the rules from drifting.

/// The declared type of an attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrType {
    String,
    StringList,
    /// A single target label (written as a string in `BUILD`, e.g. `"//pkg:t"`).
    Label,
    /// A list of target labels.
    LabelList,
    Int,
    Bool,
    /// A table (`{ "key": value, … }`) whose structure the *rule* validates, not the
    /// schema (cf. `nickel_eval` validating `format`). The loader checks only that the
    /// value is a dict; the rule reads it via [`crate::AttrValue`] accessors.
    Dict,
    /// A table from target **labels** to **strings** (`{ "//pkg:t": "dest" }`). Keys are
    /// parsed as labels and become dependency edges (like [`AttrType::LabelList`]); each
    /// value is per-edge consumer-side metadata (e.g. a routing destination, §5.4).
    LabelKeyedStringDict,
}

/// One attribute in a rule's schema. `name` is implicit (handled uniformly by the
/// loader) and is therefore not part of any rule's schema.
#[derive(Debug, Clone, Copy)]
pub struct AttrSchema {
    pub name: &'static str,
    pub ty: AttrType,
    pub required: bool,
}

impl AttrSchema {
    pub const fn required(name: &'static str, ty: AttrType) -> Self {
        AttrSchema {
            name,
            ty,
            required: true,
        }
    }

    pub const fn optional(name: &'static str, ty: AttrType) -> Self {
        AttrSchema {
            name,
            ty,
            required: false,
        }
    }
}
