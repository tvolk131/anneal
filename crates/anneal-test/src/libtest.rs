//! Translation from libtest's human-readable output into structured cases (§12.4).
//!
//! libtest prints one line per case (`test <name> ... ok|FAILED|ignored`), failure
//! detail sections (`---- <name> stdout ----`), and a summary line
//! (`test result: ok. N passed; M failed; …; finished in T s`). This parser is
//! tolerant: unrecognized lines are ignored.

use std::collections::BTreeMap;

use crate::{TestCase, TestOutcome};

/// Parsed libtest output: the cases plus the summary counts and total duration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibtestReport {
    pub cases: Vec<TestCase>,
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
    pub duration_ms: u64,
}

/// Parse libtest human output.
pub fn parse_libtest(output: &str) -> LibtestReport {
    let lines: Vec<&str> = output.lines().collect();
    let mut cases = Vec::new();
    let mut failures: BTreeMap<String, String> = BTreeMap::new();
    let mut passed = 0;
    let mut failed = 0;
    let mut ignored = 0;
    let mut duration_ms = 0;

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        if let Some(rest) = line.strip_prefix("test result:") {
            let summary = parse_summary(rest);
            passed = summary.0;
            failed = summary.1;
            ignored = summary.2;
            duration_ms = summary.3;
        } else if let Some((name, outcome)) = parse_case_line(line) {
            cases.push(TestCase {
                name,
                outcome,
                duration_ms: 0,
                failure_message: None,
            });
        } else if let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|s| s.strip_suffix(" stdout ----"))
        {
            // Collect the failure detail until the next structural line.
            let mut message = String::new();
            i += 1;
            while i < lines.len() {
                let l = lines[i];
                if l.starts_with("---- ") || l.starts_with("failures:") || l.starts_with("test result:") {
                    break;
                }
                message.push_str(l);
                message.push('\n');
                i += 1;
            }
            failures.insert(name.to_owned(), message.trim().to_owned());
            continue; // `i` already advanced past the section
        }
        i += 1;
    }

    for case in &mut cases {
        if case.outcome == TestOutcome::Failed {
            case.failure_message = failures.get(&case.name).cloned();
        }
    }

    LibtestReport {
        cases,
        passed,
        failed,
        ignored,
        duration_ms,
    }
}

/// Parse a `test <name> ... <result>` line. Returns `None` for non-case lines
/// (including the `test result:` summary and benchmarks).
fn parse_case_line(line: &str) -> Option<(String, TestOutcome)> {
    if line.starts_with("test result:") {
        return None;
    }
    let body = line.strip_prefix("test ")?;
    let (name, result) = body.rsplit_once(" ... ")?;
    let outcome = match result {
        "ok" => TestOutcome::Passed,
        "FAILED" => TestOutcome::Failed,
        r if r.starts_with("ignored") => TestOutcome::Skipped,
        _ => return None, // benches and other shapes are out of scope
    };
    Some((name.to_owned(), outcome))
}

/// Parse the summary tail (everything after `test result:`): counts and duration.
fn parse_summary(rest: &str) -> (usize, usize, usize, u64) {
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let (mut passed, mut failed, mut ignored, mut duration_ms) = (0, 0, 0, 0);

    for pair in tokens.windows(2) {
        let count: Result<usize, _> = pair[0].parse();
        let label = pair[1].trim_end_matches([';', '.']);
        if let Ok(n) = count {
            match label {
                "passed" => passed = n,
                "failed" => failed = n,
                "ignored" => ignored = n,
                _ => {}
            }
        }
    }

    // The duration token looks like `0.12s`.
    for token in &tokens {
        let token = token.trim_end_matches([';', '.']);
        if let Some(num) = token.strip_suffix('s') {
            if let Ok(secs) = num.parse::<f64>() {
                duration_ms = (secs * 1000.0) as u64;
            }
        }
    }

    (passed, failed, ignored, duration_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_PASS: &str = "\nrunning 2 tests\n\
        test tests::adds ... ok\n\
        test tests::also ... ok\n\
        \n\
        test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";

    const ONE_FAIL: &str = "\nrunning 2 tests\n\
        test tests::adds ... ok\n\
        test tests::broken ... FAILED\n\
        \n\
        failures:\n\
        \n\
        ---- tests::broken stdout ----\n\
        thread 'tests::broken' panicked at src/lib.rs:9:9:\n\
        assertion `left == right` failed\n\
          left: 5\n\
         right: 4\n\
        \n\
        \n\
        failures:\n\
            tests::broken\n\
        \n\
        test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s\n";

    const WITH_IGNORED: &str = "\nrunning 2 tests\n\
        test tests::adds ... ok\n\
        test tests::wip ... ignored\n\
        \n\
        test result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";

    #[test]
    fn parses_all_passing() {
        let r = parse_libtest(ALL_PASS);
        assert_eq!((r.passed, r.failed, r.ignored), (2, 0, 0));
        assert_eq!(r.duration_ms, 10);
        assert_eq!(r.cases.len(), 2);
        assert!(r.cases.iter().all(|c| c.outcome == TestOutcome::Passed));
        assert_eq!(r.cases[0].name, "tests::adds");
    }

    #[test]
    fn parses_a_failure_with_message() {
        let r = parse_libtest(ONE_FAIL);
        assert_eq!((r.passed, r.failed), (1, 1));
        let broken = r.cases.iter().find(|c| c.name == "tests::broken").unwrap();
        assert_eq!(broken.outcome, TestOutcome::Failed);
        let msg = broken.failure_message.as_ref().unwrap();
        assert!(msg.contains("assertion"), "captured failure detail: {msg:?}");
        assert!(msg.contains("left: 5"));
    }

    #[test]
    fn parses_ignored_as_skipped() {
        let r = parse_libtest(WITH_IGNORED);
        let wip = r.cases.iter().find(|c| c.name == "tests::wip").unwrap();
        assert_eq!(wip.outcome, TestOutcome::Skipped);
        assert_eq!(r.ignored, 1);
    }

    #[test]
    fn tolerates_zero_tests() {
        let r = parse_libtest("\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n");
        assert!(r.cases.is_empty());
        assert_eq!(r.failed, 0);
    }
}
