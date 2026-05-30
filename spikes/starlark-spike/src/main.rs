//! Spike A — starlark-rust integration.
//!
//! Retires the §22 risk "starlark-rust integration ... not yet verified end-to-end".
//! Proves the four things the loading layer (§4) needs:
//!   1. Load + evaluate a BUILD-shaped file.
//!   2. Register a rule primitive as a Starlark global that COLLECTS invocations
//!      (this is how `cargo_workspace(...)` etc. populate the target graph).
//!   3. On an evaluation error, recover a SOURCE LOCATION (file/line/col) pointing
//!      at the user's BUILD file (§17.1 structured errors point at user content).
//!   4. Inject a CUSTOM error from a rule primitive and see it surface with a span.
//!
//! Throwaway code: clarity over abstraction.

use std::cell::RefCell;

use starlark::any::ProvidesStaticType;
use starlark::environment::{GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::none::NoneType;

/// One collected rule invocation from a BUILD file.
#[derive(Debug)]
struct RuleInvocation {
    rule: String,
    name: String,
}

/// Per-evaluation sink the rule primitives write into. Stored as a Starlark "extra"
/// so globals can reach it during evaluation. This is the spike's stand-in for the
/// real target-graph builder.
#[derive(Default, ProvidesStaticType)]
struct TargetSink {
    invocations: RefCell<Vec<RuleInvocation>>,
}

#[starlark_module]
fn build_globals(builder: &mut GlobalsBuilder) {
    /// A stand-in rule primitive: `cargo_workspace(name = "...")`.
    fn cargo_workspace(
        #[starlark(require = named)] name: String,
        eval: &mut Evaluator,
    ) -> anyhow::Result<NoneType> {
        // Validate at the rule boundary (§4.3): empty name is a user error.
        if name.is_empty() {
            return Err(anyhow::anyhow!("cargo_workspace: `name` must not be empty"));
        }
        let sink = eval
            .extra
            .unwrap()
            .downcast_ref::<TargetSink>()
            .expect("TargetSink extra present");
        sink.invocations.borrow_mut().push(RuleInvocation {
            rule: "cargo_workspace".to_owned(),
            name,
        });
        Ok(NoneType)
    }

    /// A second primitive so we can show multiple rule kinds collected.
    fn nickel_eval(
        #[starlark(require = named)] name: String,
        eval: &mut Evaluator,
    ) -> anyhow::Result<NoneType> {
        let sink = eval
            .extra
            .unwrap()
            .downcast_ref::<TargetSink>()
            .expect("TargetSink extra present");
        sink.invocations.borrow_mut().push(RuleInvocation {
            rule: "nickel_eval".to_owned(),
            name,
        });
        Ok(NoneType)
    }
}

/// Evaluate one BUILD-shaped source string, returning collected invocations or an
/// error. `filename` is what appears in diagnostics.
fn eval_build(filename: &str, src: &str) -> Result<Vec<RuleInvocation>, starlark::Error> {
    let ast = AstModule::parse(filename, src.to_owned(), &Dialect::Standard)?;
    let globals = GlobalsBuilder::standard().with(build_globals).build();
    let sink = TargetSink::default();
    Module::with_temp_heap(|module| {
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&sink);
        eval.eval_module(ast, &globals)?;
        starlark::Result::Ok(())
    })?;
    Ok(sink.invocations.into_inner())
}

/// Print a starlark::Error with whatever source-location info it carries.
fn report_error(label: &str, err: &starlark::Error) {
    println!("[{label}] error surfaced:");
    // The Display impl renders the compiler-style diagnostic with the span.
    for line in format!("{err}").lines() {
        println!("    {line}");
    }
    // Also show that we can reach the span programmatically (for §17.1 structured errors).
    match err.span() {
        Some(span) => println!("    -> programmatic span: {span}"),
        None => println!("    -> (no span attached)"),
    }
}

fn main() {
    println!("== Spike A: starlark-rust integration ==\n");

    // --- 1 & 2. Load + evaluate a valid BUILD file; collect rule invocations ---
    let ok_src = r#"
cargo_workspace(name = "core")
nickel_eval(name = "config")
cargo_workspace(name = "cli")
"#;
    println!("[1+2] evaluating a valid BUILD file, collecting rule invocations:");
    match eval_build("crates/BUILD", ok_src) {
        Ok(invocations) => {
            for inv in &invocations {
                println!("    {}(name = {:?})", inv.rule, inv.name);
            }
            println!("    -> collected {} targets", invocations.len());
        }
        Err(e) => report_error("1+2", &e),
    }

    // --- 3. Evaluation error -> source location pointing at the user's BUILD file ---
    // Reference an undefined symbol; starlark should report file:line:col.
    let bad_ref_src = r#"
cargo_workspace(name = "core")
cargo_workspace(name = undefined_variable)
"#;
    println!("\n[3] undefined symbol -> expect a span at the user's BUILD file:");
    match eval_build("crates/BUILD", bad_ref_src) {
        Ok(_) => println!("    UNEXPECTED: evaluation succeeded"),
        Err(e) => report_error("3", &e),
    }

    // --- 4. Custom rule-boundary error (empty name) surfaces with a span ---
    let bad_arg_src = r#"
cargo_workspace(name = "")
"#;
    println!("\n[4] custom validation error from a rule primitive:");
    match eval_build("crates/BUILD", bad_arg_src) {
        Ok(_) => println!("    UNEXPECTED: evaluation succeeded"),
        Err(e) => report_error("4", &e),
    }

    println!("\n== done ==");
}
