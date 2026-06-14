//! `execute_graph` schedules an action DAG concurrently. These tests pin the
//! behaviors that distinguish it from a sequential loop: the input slice need not be
//! topologically ordered (dependencies are derived from edges), independent actions
//! genuinely overlap, multi-dependency joins wait for all parents, and **failure is
//! per-action data**: a non-zero exit is a normal result whose transitive dependents
//! complete as *skipped* (carrying the root failure's name) while independent
//! subgraphs keep running — only infrastructure errors abort the run.

use anneal_exec::{Action, ActionBuilder, ExecError, ExecutionMode, LocalExecutor};

/// A permeable shell action. These tests exercise scheduling behavior, and one test
/// intentionally coordinates through a host temp directory.
fn shell(name: impl Into<String>, cmd: impl Into<String>) -> ActionBuilder {
    Action::builder(name, ["/bin/sh".to_owned(), "-c".to_owned(), cmd.into()])
        .mode(ExecutionMode::Permeable)
}

/// A leaf action that writes a fixed line to a declared output.
fn writes(name: &str, out: &str, line: &str) -> Action {
    shell(name, format!("printf '{line}' > {out}"))
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
    let b = shell("B", "cat a.txt > b.txt; printf 'B\\n' >> b.txt")
        .input_from_output("a", "a.txt", "A", "a.txt")
        .output("b.txt", "b.txt")
        .build();
    let c = shell("C", "cat a.txt > c.txt; printf 'C\\n' >> c.txt")
        .input_from_output("a", "a.txt", "A", "a.txt")
        .output("c.txt", "c.txt")
        .build();
    let d = shell("D", "cat b.txt c.txt > d.txt")
        .input_from_output("b", "b.txt", "B", "b.txt")
        .input_from_output("c", "c.txt", "C", "c.txt")
        .output("d.txt", "d.txt")
        .build();

    // Scrambled: a consumer appears before its producers.
    let results = exec.execute_graph(&[d, c, a, b]).unwrap();
    assert_eq!(results.len(), 4);
    assert!(
        results.iter().all(|r| r.success()),
        "every action should succeed"
    );

    // results[0] is D (slice index 0), whatever order it actually ran in.
    let d_out = exec
        .cas()
        .get(results[0].outputs.get("d.txt").unwrap())
        .unwrap()
        .unwrap();
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
            shell(format!("rv{i}"), cmd).build()
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
fn a_failed_dependency_skips_its_dependents() {
    // The producer exits non-zero and so produces no output. Its consumer never
    // runs: it completes as *skipped*, naming the failed producer — and the run
    // returns Ok, because a build failure is per-action data, not a graph error.
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    let producer = shell("p", "exit 1").output("out", "out.txt").build();
    let consumer = shell("c", "cat got.txt > final.txt")
        .input_from_output("g", "got.txt", "p", "out")
        .output("final", "final.txt")
        .build();

    let results = exec.execute_graph(&[producer, consumer]).unwrap();
    assert_eq!(results[0].exit_code, 1);
    assert!(results[0].skipped_dependency.is_none(), "p actually ran");
    assert!(!results[1].success());
    assert_eq!(results[1].skipped_dependency.as_deref(), Some("p"));
    assert!(results[1].outputs.is_empty());
}

#[test]
fn skips_cascade_to_the_root_cause_while_independent_work_completes() {
    // fail → mid → leaf: both dependents are skipped, and *both* name the root
    // failure (not the nearest skipped link). An unrelated action still runs.
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    let root = shell("root", "exit 3").output("r", "r.txt").build();
    let mid = shell("mid", "cat r.txt > m.txt")
        .input_from_output("r", "r.txt", "root", "r")
        .output("m", "m.txt")
        .build();
    let leaf = shell("leaf", "cat m.txt > l.txt")
        .input_from_output("m", "m.txt", "mid", "m")
        .output("l", "l.txt")
        .build();
    let unrelated = writes("unrelated", "u.txt", "fine");

    let results = exec.execute_graph(&[root, mid, leaf, unrelated]).unwrap();
    assert_eq!(results[0].exit_code, 3);
    assert_eq!(results[1].skipped_dependency.as_deref(), Some("root"));
    assert_eq!(
        results[2].skipped_dependency.as_deref(),
        Some("root"),
        "the leaf names the root failure, not the skipped mid link"
    );
    assert!(
        results[3].success(),
        "independent work completes despite the failure"
    );
}

#[test]
fn a_join_with_one_failed_parent_is_skipped() {
    // join needs both parents; one fails, one succeeds → join is skipped and
    // names the failed parent.
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    let ok = writes("ok", "a.txt", "fine");
    let bad = shell("bad", "exit 1").output("b", "b.txt").build();
    let join = shell("join", "cat a.txt b.txt > j.txt")
        .input_from_output("a", "a.txt", "ok", "a.txt")
        .input_from_output("b", "b.txt", "bad", "b")
        .output("j", "j.txt")
        .build();

    let results = exec.execute_graph(&[ok, bad, join]).unwrap();
    assert!(results[0].success());
    assert!(!results[1].success());
    assert_eq!(results[2].skipped_dependency.as_deref(), Some("bad"));
}

#[test]
fn a_nonzero_exit_leaf_is_a_result_not_an_abort() {
    // A failing action with no dependents is a normal (non-success) result — the rest
    // of the graph still runs and the graph call returns Ok.
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    let good = writes("good", "g.txt", "ok");
    let bad = shell("bad", "exit 7").build();

    let results = exec.execute_graph(&[good, bad]).unwrap();
    assert!(results[0].success(), "the good leaf succeeds");
    assert!(
        !results[1].success(),
        "the bad leaf is a non-success result"
    );
    assert_eq!(results[1].exit_code, 7);
}

#[test]
fn a_dependency_cycle_is_detected() {
    // Two actions referencing each other's output: no topological order exists, so the
    // scheduler reports a cycle rather than hanging.
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    let x = shell("x", "true")
        .input_from_output("from_y", "y.txt", "y", "o")
        .output("ox", "x.txt")
        .build();
    let y = shell("y", "true")
        .input_from_output("from_x", "x.txt", "x", "ox")
        .output("o", "y.txt")
        .build();

    let err = exec.execute_graph(&[x, y]).unwrap_err();
    assert!(
        matches!(err, ExecError::DependencyCycle),
        "expected DependencyCycle, got {err:?}"
    );
}

#[test]
fn an_empty_graph_is_ok() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();
    assert!(exec.execute_graph(&[]).unwrap().is_empty());
}

#[test]
fn a_failed_action_carries_a_bounded_output_tail() {
    // The failure report needs the *why*: stderr is captured and a tail rides
    // on the result. Successful actions carry none (no memory tax), and a
    // tool that reports errors on stdout still gets a tail (the fallback).
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();

    let stderr_fail = shell("stderr-fail", "echo boom-on-stderr >&2; exit 1").build();
    let stdout_fail = shell("stdout-fail", "echo boom-on-stdout; exit 1").build();
    let quiet_ok = writes("ok", "o.txt", "fine");

    let results = exec
        .execute_graph(&[stderr_fail, stdout_fail, quiet_ok])
        .unwrap();
    assert!(
        results[0]
            .failure_output
            .as_deref()
            .is_some_and(|t| t.contains("boom-on-stderr")),
        "stderr tail missing: {:?}",
        results[0].failure_output
    );
    assert!(
        results[1]
            .failure_output
            .as_deref()
            .is_some_and(|t| t.contains("boom-on-stdout")),
        "stdout fallback tail missing: {:?}",
        results[1].failure_output
    );
    assert!(
        results[2].failure_output.is_none(),
        "success carries no output tail"
    );
}
