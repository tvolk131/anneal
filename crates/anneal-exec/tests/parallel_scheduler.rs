//! `execute_graph` schedules an action DAG concurrently. These tests pin the
//! behaviors that distinguish it from a sequential loop: the input slice need not be
//! topologically ordered (dependencies are derived from edges), independent actions
//! genuinely overlap, multi-dependency joins wait for all parents, and execution
//! *errors* abort the run while a non-zero *exit* is a normal result.

use anneal_exec::{Action, ExecError, LocalExecutor};

/// `/bin/sh -c <cmd>` as an owned argv (so a `format!`-built command type-checks).
fn sh(cmd: String) -> Vec<String> {
    vec!["/bin/sh".to_owned(), "-c".to_owned(), cmd]
}

/// A leaf action that writes a fixed line to a declared output.
fn writes(name: &str, out: &str, line: &str) -> Action {
    Action::builder(name, sh(format!("printf '{line}' > {out}")))
        .output(out, out)
        .build()
}

#[test]
fn input_order_need_not_be_topological() {
    // A diamond A → {B, C} → D, handed to the scheduler **out of order** ([D, C, A, B]).
    // The result must be correct and aligned with the *input* order regardless.
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    let a = writes("A", "a.txt", "A\n");
    let b = Action::builder("B", ["/bin/sh", "-c", "cat a.txt > b.txt; printf 'B\\n' >> b.txt"])
        .input_from_output("a", "a.txt", "A", "a.txt")
        .output("b.txt", "b.txt")
        .build();
    let c = Action::builder("C", ["/bin/sh", "-c", "cat a.txt > c.txt; printf 'C\\n' >> c.txt"])
        .input_from_output("a", "a.txt", "A", "a.txt")
        .output("c.txt", "c.txt")
        .build();
    let d = Action::builder("D", ["/bin/sh", "-c", "cat b.txt c.txt > d.txt"])
        .input_from_output("b", "b.txt", "B", "b.txt")
        .input_from_output("c", "c.txt", "C", "c.txt")
        .output("d.txt", "d.txt")
        .build();

    // Scrambled: a consumer appears before its producers.
    let results = exec.execute_graph(&[d, c, a, b]).unwrap();
    assert_eq!(results.len(), 4);
    assert!(results.iter().all(|r| r.success()), "every action should succeed");

    // results[0] is D (slice index 0), whatever order it actually ran in.
    let d_out = exec.cas().get(results[0].outputs.get("d.txt").unwrap()).unwrap().unwrap();
    assert_eq!(String::from_utf8(d_out).unwrap(), "A\nB\nA\nC\n");
}

#[test]
fn independent_actions_actually_overlap() {
    // A rendezvous proves real concurrency: each of N actions touches a marker in a
    // shared dir, then waits until all N markers exist. If the scheduler ran them
    // sequentially, the first would wait forever (the others never start) and time out
    // to a non-zero exit. Success for all N is only possible if they run at once.
    const N: usize = 4;
    let dir = tempfile::tempdir().unwrap();
    let rv = dir.path().join("rendezvous");
    std::fs::create_dir_all(&rv).unwrap();
    let rv = rv.display();

    let exec = LocalExecutor::new(dir.path()).unwrap().jobs(N);
    let actions: Vec<Action> = (0..N)
        .map(|i| {
            let cmd = format!(
                "touch {rv}/{i}; \
                 c=0; while [ \"$(ls {rv} | wc -l)\" -lt {N} ] && [ \"$c\" -lt 100 ]; \
                 do sleep 0.05; c=$((c+1)); done; \
                 [ \"$(ls {rv} | wc -l)\" -ge {N} ]"
            );
            Action::builder(format!("rv{i}"), sh(cmd)).build()
        })
        .collect();

    let results = exec.execute_graph(&actions).unwrap();
    assert_eq!(results.len(), N);
    assert!(
        results.iter().all(|r| r.success()),
        "all actions must reach the rendezvous, which requires genuine concurrency"
    );
}

#[test]
fn an_execution_error_aborts_dependents() {
    // The producer exits non-zero and so produces no output. Its consumer references
    // that missing output → the run fails with UnresolvedInput (an execution error),
    // not a silent empty result.
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    let producer = Action::builder("p", ["/bin/sh", "-c", "exit 1"])
        .output("out", "out.txt")
        .build();
    let consumer = Action::builder("c", ["/bin/sh", "-c", "cat got.txt > final.txt"])
        .input_from_output("g", "got.txt", "p", "out")
        .output("final", "final.txt")
        .build();

    let err = exec.execute_graph(&[producer, consumer]).unwrap_err();
    assert!(
        matches!(&err, ExecError::UnresolvedInput { action, output } if action == "p" && output == "out"),
        "expected UnresolvedInput for p's output, got {err:?}"
    );
}

#[test]
fn a_nonzero_exit_leaf_is_a_result_not_an_abort() {
    // A failing action with no dependents is a normal (non-success) result — the rest
    // of the graph still runs and the graph call returns Ok.
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    let good = writes("good", "g.txt", "ok");
    let bad = Action::builder("bad", ["/bin/sh", "-c", "exit 7"]).build();

    let results = exec.execute_graph(&[good, bad]).unwrap();
    assert!(results[0].success(), "the good leaf succeeds");
    assert!(!results[1].success(), "the bad leaf is a non-success result");
    assert_eq!(results[1].exit_code, 7);
}

#[test]
fn a_dependency_cycle_is_detected() {
    // Two actions referencing each other's output: no topological order exists, so the
    // scheduler reports a cycle rather than hanging.
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    let x = Action::builder("x", ["/bin/sh", "-c", "true"])
        .input_from_output("from_y", "y.txt", "y", "o")
        .output("ox", "x.txt")
        .build();
    let y = Action::builder("y", ["/bin/sh", "-c", "true"])
        .input_from_output("from_x", "x.txt", "x", "ox")
        .output("o", "y.txt")
        .build();

    let err = exec.execute_graph(&[x, y]).unwrap_err();
    assert!(matches!(err, ExecError::DependencyCycle), "expected DependencyCycle, got {err:?}");
}

#[test]
fn an_empty_graph_is_ok() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();
    assert!(exec.execute_graph(&[]).unwrap().is_empty());
}
