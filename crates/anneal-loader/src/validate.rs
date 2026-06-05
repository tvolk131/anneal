//! Schema validation and coercion: a [`RawTarget`] becomes a typed [`TargetDecl`]
//! (§4.3). This is where string-typed label attributes become real [`Label`]s and
//! where dependency edges are extracted from label-typed attributes.

use std::collections::BTreeSet;

use anneal_core::Label;
use anneal_rules::{AttrType, AttrValue, Attrs, RuleRegistry};

use crate::error::LoadError;
use crate::eval::{RawTarget, RawValue};
use crate::graph::TargetDecl;

pub(crate) fn build_target(
    package: &str,
    raw: RawTarget,
    registry: &RuleRegistry,
) -> Result<TargetDecl, LoadError> {
    let location = raw.location.clone();

    let rule = registry.get(raw.kind).ok_or_else(|| {
        LoadError::schema(
            format!("unknown rule kind `{}`", raw.kind),
            location.clone(),
        )
    })?;
    let schema = rule.schema();

    let label = Label::parse(&format!("//{package}:{}", raw.name)).map_err(|e| {
        LoadError::schema(
            format!("invalid target name {:?}: {e}", raw.name),
            location.clone(),
        )
    })?;

    // Reject unknown attributes.
    let known: BTreeSet<&str> = schema.iter().map(|a| a.name).collect();
    for key in raw.attrs.keys() {
        if !known.contains(key.as_str()) {
            return Err(LoadError::schema(
                format!("{label}: unknown attribute `{key}` for rule `{}`", raw.kind),
                location,
            ));
        }
    }

    // Validate and coerce each declared attribute.
    let mut builder = Attrs::builder();
    let mut deps = Vec::new();
    for attr in schema {
        match raw.attrs.get(attr.name) {
            None if attr.required => {
                return Err(LoadError::schema(
                    format!("{label}: missing required attribute `{}`", attr.name),
                    location,
                ));
            }
            None => {}
            Some(raw_value) => {
                let (value, attr_deps) = coerce(attr.ty, raw_value).map_err(|msg| {
                    LoadError::schema(
                        format!("{label}: attribute `{}` {msg}", attr.name),
                        location.clone(),
                    )
                })?;
                deps.extend(attr_deps);
                builder = builder.value(attr.name, value);
            }
        }
    }

    Ok(TargetDecl {
        label,
        kind: raw.kind.to_owned(),
        attrs: builder.build(),
        deps,
        location,
    })
}

/// Coerce a raw value to the schema-declared type, returning the typed value and any
/// dependency labels it introduces (for `Label`/`LabelList` attributes).
fn coerce(ty: AttrType, raw: &RawValue) -> Result<(AttrValue, Vec<Label>), String> {
    match (ty, raw) {
        (AttrType::String, RawValue::String(s)) => Ok((AttrValue::String(s.clone()), Vec::new())),
        (AttrType::Int, RawValue::Int(i)) => Ok((AttrValue::Int(*i), Vec::new())),
        (AttrType::Bool, RawValue::Bool(b)) => Ok((AttrValue::Bool(*b), Vec::new())),
        (AttrType::StringList, RawValue::StringList(v)) => {
            Ok((AttrValue::StringList(v.clone()), Vec::new()))
        }
        (AttrType::Label, RawValue::String(s)) => {
            let label = Label::parse(s).map_err(|e| format!("must be a valid label: {e}"))?;
            Ok((AttrValue::Label(label.clone()), vec![label]))
        }
        (AttrType::LabelList, RawValue::StringList(v)) => {
            let mut labels = Vec::with_capacity(v.len());
            for s in v {
                labels
                    .push(Label::parse(s).map_err(|e| format!("must contain valid labels: {e}"))?);
            }
            Ok((AttrValue::LabelList(labels.clone()), labels))
        }
        // A dict is passed through structurally — the rule validates its contents
        // (§4.3 puts domain validation in the rule). Tables introduce no dep edges.
        (AttrType::Dict, RawValue::Dict(_)) => Ok((natural(raw), Vec::new())),
        // A label-keyed string dict: keys parse as labels (→ dep edges, like LabelList),
        // values must be strings.
        (AttrType::LabelKeyedStringDict, RawValue::Dict(m)) => {
            let mut pairs = Vec::with_capacity(m.len());
            let mut labels = Vec::with_capacity(m.len());
            for (key, value) in m {
                let label = Label::parse(key)
                    .map_err(|e| format!("key `{key}` must be a valid label: {e}"))?;
                let RawValue::String(s) = value else {
                    return Err(format!("value for `{key}` must be a string"));
                };
                labels.push(label.clone());
                pairs.push((label, s.clone()));
            }
            Ok((AttrValue::LabelKeyedStringDict(pairs), labels))
        }
        (expected, _) => Err(format!("must be a {}", type_name(expected))),
    }
}

/// Convert a raw value to its natural typed form with **no schema coercion** — used
/// for the contents of a [`AttrType::Dict`], whose inner values the rule interprets.
/// String→label coercion does not apply inside tables (they carry no dep edges).
fn natural(raw: &RawValue) -> AttrValue {
    match raw {
        RawValue::String(s) => AttrValue::String(s.clone()),
        RawValue::Int(i) => AttrValue::Int(*i),
        RawValue::Bool(b) => AttrValue::Bool(*b),
        RawValue::StringList(v) => AttrValue::StringList(v.clone()),
        RawValue::Dict(m) => {
            AttrValue::Dict(m.iter().map(|(k, v)| (k.clone(), natural(v))).collect())
        }
    }
}

fn type_name(ty: AttrType) -> &'static str {
    match ty {
        AttrType::String => "string",
        AttrType::StringList => "list of strings",
        AttrType::Label => "label",
        AttrType::LabelList => "list of labels",
        AttrType::Int => "int",
        AttrType::Bool => "bool",
        AttrType::Dict => "dict",
        AttrType::LabelKeyedStringDict => "label-keyed string dict",
    }
}
