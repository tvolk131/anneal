//! `anneal-test` — structured test results (§12.4, §19.2).
//!
//! Tests are a distinct concept with structured per-case outcomes, not just a
//! pass/fail exit code. A test runner emits a framework-native format; this crate
//! holds the system's [`TestResult`] schema and the **translation** from each
//! framework's output into it ([`libtest`] for Rust). Case identity
//! (`test_target + name`) is stable across builds so external history/flake tooling
//! stays possible — settled now even though sharding and flakiness retries are v1.x
//! (§12.5).
//!
//! ## Milestone 1 fidelity
//!
//! libtest's JSON output is nightly-only, so on stable Rust we parse its **human**
//! output. That yields per-case outcomes, failure messages, and the total duration,
//! but **not** per-case durations (`duration_ms` is 0 per case) — a documented
//! limitation until a JSON-capable path exists.

mod libtest;

pub use libtest::{parse_libtest, LibtestReport};

use anneal_core::{Configuration, Label};

/// The outcome of a test or a single case (§19.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestOutcome {
    Passed,
    Failed,
    Skipped,
    Errored,
    TimedOut,
    /// v1.x flakiness retries; never produced in Milestone 1.
    PassedAfterRetry,
}

/// One test case's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
    /// Framework-local name, e.g. `tests::adds`. With the enclosing
    /// [`TestResult::test_target`] this forms the stable case identity.
    pub name: String,
    pub outcome: TestOutcome,
    /// Per-case duration. 0 in Milestone 1 (unavailable from libtest's stable output).
    pub duration_ms: u64,
    pub failure_message: Option<String>,
}

/// The structured result of running one test target (§19.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestResult {
    pub test_target: Label,
    pub configuration: Configuration,
    pub outcome: TestOutcome,
    pub duration_ms: u64,
    pub cases: Vec<TestCase>,
    /// v1.x retries; 0 in Milestone 1.
    pub retry_count: u32,
}

impl TestResult {
    /// Translate libtest human output into a structured result for `test_target`.
    pub fn from_libtest(
        test_target: Label,
        configuration: Configuration,
        output: &str,
    ) -> TestResult {
        let report = parse_libtest(output);
        // Target outcome: any failed case fails the target; otherwise it passed.
        let outcome = if report.failed > 0 {
            TestOutcome::Failed
        } else {
            TestOutcome::Passed
        };
        TestResult {
            test_target,
            configuration,
            outcome,
            duration_ms: report.duration_ms,
            cases: report.cases,
            retry_count: 0,
        }
    }

    /// The stable identity of a case: `//target:name#case_name`.
    pub fn case_id(&self, case: &TestCase) -> String {
        format!("{}#{}", self.test_target, case.name)
    }

    /// Count of cases with a given outcome.
    pub fn count(&self, outcome: TestOutcome) -> usize {
        self.cases.iter().filter(|c| c.outcome == outcome).count()
    }
}
