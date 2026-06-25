//! Native fixed-output fetch (§FOD): the executor downloads pinned blobs
//! in-process — no curl, no sandbox, no host trust configuration — verifies
//! them against the pin (fail closed), and caches by output. Exercised against
//! a local HTTP server, so these tests are hermetic.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::thread;

use anneal_core::Digest;
use anneal_exec::{Action, ExecError, Executor, LocalExecutor};

/// Serve each canned response to one connection, in order, then stop. Returns
/// the base URL and a handle yielding how many connections were served.
fn serve(responses: Vec<Vec<u8>>) -> (String, thread::JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut served = 0;
        for response in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request); // request content is irrelevant
            let _ = stream.write_all(&response);
            served += 1;
        }
        served
    });
    (format!("http://{addr}"), handle)
}

fn http_response(status: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn fetch_action(name: &str, url: &str, expected: Digest) -> Action {
    Action::builder(name, Vec::<String>::new())
        .output("crate", PathBuf::from("blob.crate"))
        .platform_independent()
        .fetch(url, expected)
        .try_build()
        .unwrap()
}

#[test]
fn downloads_verifies_admits_to_cas_and_then_hits_without_network() {
    let body = b"crate bytes".to_vec();
    let expected = Digest::of(&body);
    let (base, server) = serve(vec![http_response("200 OK", &body)]);

    let tmp = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(tmp.path().join(".anneal")).unwrap();
    let action = fetch_action("fetch ok", &format!("{base}/x.crate"), expected);

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert!(!result.cache_hit);
    assert_eq!(result.outputs["crate"], expected);
    assert_eq!(exec.cas().get(&expected).unwrap().unwrap(), body);
    assert_eq!(server.join().unwrap(), 1);

    // Cached by output: the second execute never touches the network (the
    // server is gone) and reports a cache hit.
    let result = exec.execute(&action).unwrap();
    assert!(result.cache_hit);
    assert_eq!(result.outputs["crate"], expected);
}

#[test]
fn hash_mismatch_fails_closed() {
    let (base, _server) = serve(vec![http_response("200 OK", b"not what was pinned")]);
    let expected = Digest::of(b"what was pinned");

    let tmp = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(tmp.path().join(".anneal")).unwrap();
    let action = fetch_action("fetch mismatch", &format!("{base}/x.crate"), expected);

    match exec.execute(&action) {
        Err(ExecError::FixedOutputMismatch {
            expected: e,
            actual,
        }) => {
            assert_eq!(e, expected);
            assert_eq!(actual, Digest::of(b"not what was pinned"));
        }
        other => panic!("expected FixedOutputMismatch, got {other:?}"),
    }
}

#[test]
fn transient_5xx_is_retried() {
    let body = b"eventually fine".to_vec();
    let expected = Digest::of(&body);
    let (base, server) = serve(vec![
        http_response("500 Internal Server Error", b"flake"),
        http_response("200 OK", &body),
    ]);

    let tmp = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(tmp.path().join(".anneal")).unwrap();
    let action = fetch_action("fetch retry", &format!("{base}/x.crate"), expected);

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    assert_eq!(server.join().unwrap(), 2, "the 500 should be retried once");
}

#[test]
fn definitive_4xx_fails_without_retry() {
    let (base, server) = serve(vec![http_response("404 Not Found", b"missing")]);

    let tmp = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(tmp.path().join(".anneal")).unwrap();
    let action = fetch_action(
        "fetch missing",
        &format!("{base}/x.crate"),
        Digest::of(b"irrelevant"),
    );

    match exec.execute(&action) {
        Err(ExecError::Fetch { action, error }) => {
            assert_eq!(action, "fetch missing");
            let message = error.to_string();
            assert!(
                message.contains("after 1 attempt(s)"),
                "a 404 must not retry: {message}"
            );
        }
        other => panic!("expected ExecError::Fetch, got {other:?}"),
    }
    assert_eq!(server.join().unwrap(), 1);
}

/// The fetch contract is validated at build time: no command, no inputs, no
/// toolchains, exactly one output.
#[test]
fn builder_rejects_malformed_fetch_actions() {
    let expected = Digest::of(b"x");

    let with_command = Action::builder("bad", vec!["sh".to_owned()])
        .output("crate", PathBuf::from("out"))
        .fetch("http://localhost/x", expected)
        .try_build();
    assert!(with_command.is_err(), "a fetch declares no command");

    let no_outputs = Action::builder("bad", Vec::<String>::new())
        .fetch("http://localhost/x", expected)
        .try_build();
    assert!(no_outputs.is_err(), "a fetch pins exactly one output");

    let two_outputs = Action::builder("bad", Vec::<String>::new())
        .output("a", PathBuf::from("a"))
        .output("b", PathBuf::from("b"))
        .fetch("http://localhost/x", expected)
        .try_build();
    assert!(two_outputs.is_err(), "a fetch pins exactly one output");

    let with_input = Action::builder("bad", Vec::<String>::new())
        .source_input("src", PathBuf::from("src"), expected)
        .output("crate", PathBuf::from("out"))
        .fetch("http://localhost/x", expected)
        .try_build();
    assert!(with_input.is_err(), "a fetch has no inputs");
}
