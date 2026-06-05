//! Phase 1 exit criterion: hand the kernel a hand-written action → get a sandboxed,
//! content-addressed, cached result; re-running the identical action is a cache hit
//! with no re-execution.

use std::collections::BTreeMap;

use anneal_core::Digest;
use anneal_exec::{Action, CachePolicy, Executor, LocalExecutor};

mod support;

/// An action that reads a declared input and writes a transformed declared output.
fn copy_upper_action(input_digest: Digest) -> Action {
    support::shell_action(
        "upcase",
        // Read the materialized input, uppercase it, write the declared output.
        "tr a-z A-Z < in.txt > out.txt",
    )
    .input("src", "in.txt", input_digest)
    .output("result", "out.txt")
    .build()
}

#[test]
fn executes_then_serves_from_cache() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    let input = exec.cas().put(b"hello kernel").unwrap();
    let action = copy_upper_action(input);

    // First run: cache miss, real execution.
    let first = exec.execute(&action).unwrap();
    assert!(first.success());
    assert!(!first.cache_hit, "first run must execute");
    let out_digest = first.outputs.get("result").copied().unwrap();
    assert_eq!(
        exec.cas().get(&out_digest).unwrap().as_deref(),
        Some(&b"HELLO KERNEL"[..]),
        "output should be the uppercased input"
    );

    // Second run of the identical action: cache hit, no re-execution, same outputs.
    let second = exec.execute(&action).unwrap();
    assert!(second.cache_hit, "second run must hit the cache");
    assert_eq!(second.outputs, first.outputs);
}

#[test]
fn changing_input_content_busts_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    let a = exec
        .execute(&copy_upper_action(exec.cas().put(b"one").unwrap()))
        .unwrap();
    let b = exec
        .execute(&copy_upper_action(exec.cas().put(b"two").unwrap()))
        .unwrap();

    assert!(
        !a.cache_hit && !b.cache_hit,
        "different inputs are different actions"
    );
    assert_ne!(
        a.outputs, b.outputs,
        "different inputs produce different outputs"
    );
}

#[test]
fn non_cacheable_action_always_reruns() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    let input = exec.cas().put(b"data").unwrap();
    let action = support::shell_action("echo", "cat in.txt > out.txt")
        .input("src", "in.txt", input)
        .output("result", "out.txt")
        .cache_policy(CachePolicy::NonCacheable)
        .build();

    let first = exec.execute(&action).unwrap();
    let second = exec.execute(&action).unwrap();
    assert!(
        !first.cache_hit && !second.cache_hit,
        "non-cacheable never hits"
    );
    // ...but it still produces correct, content-identical outputs each time.
    assert_eq!(first.outputs, second.outputs);
}

#[test]
fn environment_is_scrubbed_but_declared_vars_pass_through() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    // A variable present in THIS process must not leak into the sandbox.
    std::env::set_var("ANNEAL_TEST_LEAK", "leaked");

    let action = support::shell_action(
        "env-probe",
        // Leaked var should be empty (-> CLEAN); declared var should pass through.
        r#"printf '%s|%s' "${ANNEAL_TEST_LEAK:-CLEAN}" "${DECLARED:-MISSING}" > out.txt"#,
    )
    .env("DECLARED", "present")
    .output("result", "out.txt")
    .build();

    let result = exec.execute(&action).unwrap();
    let out = exec
        .cas()
        .get(result.outputs.get("result").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "CLEAN|present");
}

#[test]
fn missing_declared_output_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    // Command succeeds but never writes the declared output.
    let action = support::shell_action("noop", "true")
        .output("result", "out.txt")
        .build();

    let err = exec.execute(&action).unwrap_err();
    assert!(
        matches!(&err, anneal_exec::ExecError::MissingOutput(name) if name == "result"),
        "expected MissingOutput, got {err:?}"
    );
}

#[test]
fn execute_graph_threads_outputs_between_actions() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();
    let seed = exec.cas().put(b"seed\n").unwrap();

    // Producer uppercases its source into the output "produced.txt".
    let producer = support::shell_action("producer", "tr a-z A-Z < in.txt > produced.txt")
        .input("src", "in.txt", seed)
        .output("produced.txt", "produced.txt")
        .build();

    // Consumer reads the producer's output (materialized at from_producer.txt) and
    // appends a line — it references the producer by name + output name.
    let consumer = support::shell_action(
        "consumer",
        "cat from_producer.txt > final.txt; echo done >> final.txt",
    )
    .input_from_output("p", "from_producer.txt", "producer", "produced.txt")
    .output("final.txt", "final.txt")
    .build();

    let results = exec.execute_graph(&[producer, consumer]).unwrap();
    assert_eq!(results.len(), 2);
    assert!(results[0].success() && results[1].success());
    let out = exec
        .cas()
        .get(results[1].outputs.get("final.txt").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "SEED\ndone\n");
}

#[test]
fn executing_an_unresolved_action_directly_errors() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();
    let action = support::shell_action("c", "true")
        .input_from_output("p", "x.txt", "no_such_producer", "out")
        .output("o", "o.txt")
        .build();
    let err = exec.execute(&action).unwrap_err();
    assert!(matches!(
        err,
        anneal_exec::ExecError::UnresolvedInput { .. }
    ));
}

#[test]
fn failed_action_reports_exit_code_and_is_not_cached() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    let action = support::shell_action("fail", "exit 3").build();

    let result = exec.execute(&action).unwrap();
    assert_eq!(result.exit_code, 3);
    assert!(!result.success());
    assert_eq!(result.outputs, BTreeMap::new());

    // A subsequent run executes again (failures are not cached) and fails the same way.
    let again = exec.execute(&action).unwrap();
    assert!(!again.cache_hit);
    assert_eq!(again.exit_code, 3);
}
