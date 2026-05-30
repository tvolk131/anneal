//! Starlark evaluation, isolated here so the rest of the crate never sees a Starlark
//! type. Rule primitives are registered as globals; each call records a [`RawTarget`]
//! — name, kind, raw attribute values, and source location — into a collector.
//!
//! Attribute values are converted to a schema-agnostic [`RawValue`] here (mirroring
//! the Starlark types written in the file). Schema application — required/unknown
//! checks and string→label coercion — happens later in `validate`, where the rule
//! registry is in hand.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::{GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use starlark::values::Value;

/// A raw attribute value, mirroring the Starlark types a `BUILD` file can write.
#[derive(Debug, Clone)]
pub(crate) enum RawValue {
    String(String),
    Int(i64),
    Bool(bool),
    StringList(Vec<String>),
}

/// A target as recorded during evaluation, before schema validation.
#[derive(Debug)]
pub(crate) struct RawTarget {
    pub kind: &'static str,
    pub name: String,
    pub attrs: BTreeMap<String, RawValue>,
    pub location: Option<String>,
}

/// Per-evaluation collector, reached by rule primitives via `eval.extra`.
#[derive(ProvidesStaticType)]
struct Collector {
    targets: RefCell<Vec<RawTarget>>,
    /// Target names already declared in this package (duplicate detection).
    seen: RefCell<BTreeSet<String>>,
}

/// Record one rule invocation. Shared by every rule-primitive global.
fn record<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    kind: &'static str,
    name: String,
    kwargs: SmallMap<String, Value<'v>>,
) -> anyhow::Result<NoneType> {
    let location = eval.call_stack_top_location().map(|span| span.to_string());

    let collector = eval
        .extra
        .expect("collector installed as eval.extra")
        .downcast_ref::<Collector>()
        .expect("eval.extra is a Collector");

    if !collector.seen.borrow_mut().insert(name.clone()) {
        return Err(anyhow::anyhow!(
            "duplicate target name {name:?} in this package"
        ));
    }

    let mut attrs = BTreeMap::new();
    for (key, value) in kwargs {
        attrs.insert(key, raw_from_value(value)?);
    }

    collector.targets.borrow_mut().push(RawTarget {
        kind,
        name,
        attrs,
        location,
    });
    Ok(NoneType)
}

/// Convert a Starlark value into a [`RawValue`]. Errors (with a source span, since we
/// are inside the rule call) on unsupported shapes.
fn raw_from_value(value: Value) -> anyhow::Result<RawValue> {
    if let Some(s) = value.unpack_str() {
        return Ok(RawValue::String(s.to_owned()));
    }
    if let Some(b) = value.unpack_bool() {
        return Ok(RawValue::Bool(b));
    }
    if let Some(i) = value.unpack_i32() {
        return Ok(RawValue::Int(i as i64));
    }
    if let Some(list) = ListRef::from_value(value) {
        let mut items = Vec::with_capacity(list.len());
        for item in list.iter() {
            match item.unpack_str() {
                Some(s) => items.push(s.to_owned()),
                None => {
                    return Err(anyhow::anyhow!(
                        "list attributes may only contain strings, found `{}`",
                        item.get_type()
                    ))
                }
            }
        }
        return Ok(RawValue::StringList(items));
    }
    Err(anyhow::anyhow!(
        "unsupported attribute value of type `{}`",
        value.get_type()
    ))
}

#[starlark_module]
fn build_globals(builder: &mut GlobalsBuilder) {
    fn genrule<'v>(
        #[starlark(require = named)] name: String,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        record(eval, "genrule", name, kwargs)
    }

    fn filegroup<'v>(
        #[starlark(require = named)] name: String,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        record(eval, "filegroup", name, kwargs)
    }

    fn alias<'v>(
        #[starlark(require = named)] name: String,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        record(eval, "alias", name, kwargs)
    }

    fn cargo_workspace<'v>(
        #[starlark(require = named)] name: String,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        record(eval, "cargo_workspace", name, kwargs)
    }

    fn nickel_eval<'v>(
        #[starlark(require = named)] name: String,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        record(eval, "nickel_eval", name, kwargs)
    }
}

/// Parse and evaluate one `BUILD` file, returning the raw target declarations.
/// `_package` is currently unused here (labels are built in `validate`) but kept for
/// symmetry and future `load()`/visibility resolution.
pub(crate) fn evaluate(
    filename: &str,
    _package: &str,
    source: &str,
) -> Result<Vec<RawTarget>, starlark::Error> {
    let ast = AstModule::parse(filename, source.to_owned(), &Dialect::Standard)?;
    let globals = GlobalsBuilder::standard().with(build_globals).build();
    let collector = Collector {
        targets: RefCell::new(Vec::new()),
        seen: RefCell::new(BTreeSet::new()),
    };
    Module::with_temp_heap(|module| {
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&collector);
        eval.eval_module(ast, &globals)?;
        starlark::Result::Ok(())
    })?;
    Ok(collector.targets.into_inner())
}
