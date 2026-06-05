//! Fixed-output (FOD) fetch actions (§FOD): cached by their **output** hash, verified
//! against a pin, network-permitted. The pin turns an impure fetch into a deterministic,
//! cacheable function — a present blob skips the fetch, and a wrong hash fails closed.
//!
//! We exercise the machinery with local shell commands; network *isolation*/enforcement
//! is a separate concern, so a `printf` stands in for a download here.

use anneal_core::Digest;
use anneal_exec::{Action, ExecError, Executor, LocalExecutor};

fn sh(cmd: String) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), cmd]
}

/// A "fetch" that writes `content` to the single declared output, pinned to `expected`.
fn fetch_action(content: &str, expected: Digest) -> Action {
    Action::builder("fetch", sh(format!("printf '%s' '{content}' > out")))
        .output("artifact", "out")
        .fixed_output(expected)
        .build()
}

#[test]
fn fetch_verifies_then_caches_by_output() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();
    let content = "crate-bytes-v1";
    let expected = Digest::of(content.as_bytes());

    // First fetch: a miss → runs, verifies against the pin, stores the blob.
    let first = exec.execute(&fetch_action(content, expected)).unwrap();
    assert!(first.success());
    assert!(!first.cache_hit, "first fetch must run");
    assert_eq!(first.outputs.get("artifact").copied(), Some(expected));

    // Second time: the pinned blob is in the CAS, so the fetch is skipped entirely.
    let second = exec.execute(&fetch_action(content, expected)).unwrap();
    assert!(second.cache_hit, "the present pin → no re-fetch");
    assert_eq!(second.outputs.get("artifact").copied(), Some(expected));
}

#[test]
fn wrong_hash_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();
    // Pin to one thing, "fetch" another: the produced digest can't match the pin.
    let expected = Digest::of(b"what we asked for");
    let err = exec
        .execute(&fetch_action("something else entirely", expected))
        .unwrap_err();
    match err {
        ExecError::FixedOutputMismatch {
            expected: e,
            actual,
        } => {
            assert_eq!(e, expected);
            assert_ne!(actual, expected);
        }
        other => panic!("expected FixedOutputMismatch, got {other:?}"),
    }
}

#[test]
fn present_pin_short_circuits_the_fetch_command() {
    // Cross-build/cross-project dedup: a blob already on the machine means the fetch
    // never runs — proven by pinning to a present blob behind a command that *fails*.
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();
    let expected = exec.cas().put(b"already-on-this-machine").unwrap();

    let action = Action::builder("fetch", sh("exit 1".into()))
        .output("artifact", "out")
        .fixed_output(expected)
        .build();

    let r = exec.execute(&action).unwrap();
    assert!(
        r.cache_hit && r.success(),
        "a present pin must short-circuit the failing fetch"
    );
    assert_eq!(r.outputs.get("artifact").copied(), Some(expected));
}

#[test]
fn fixed_output_requires_exactly_one_output() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();
    // No declared output: malformed FOD → arity error, before any fetch.
    let action = Action::builder("fetch", sh("true".into()))
        .fixed_output(Digest::of(b"x"))
        .build();
    match exec.execute(&action).unwrap_err() {
        ExecError::FixedOutputArity { outputs, .. } => assert_eq!(outputs, 0),
        other => panic!("expected FixedOutputArity, got {other:?}"),
    }
}

#[test]
fn fixed_output_enables_the_network_capability() {
    // The builder turns the capability on (the pin fences the impurity).
    let action = fetch_action("x", Digest::of(b"x"));
    assert!(
        action.allows_network(),
        "fixed_output() must permit network"
    );
    // A plain action does not.
    let plain = Action::builder("plain", sh("true".into())).build();
    assert!(!plain.allows_network(), "network is off by default");
}
