//! gtest-style console reporting for the self-test harness.
//!
//! Output mirrors Google Test so the format is familiar:
//!
//! ```text
//! [==========] Running 6 tests.
//! [ RUN      ] usb_direct.shell_echo
//! [       OK ] usb_direct.shell_echo (12 ms)
//! [ RUN      ] usb_direct.push_pull_roundtrip
//! [  FAILED  ] usb_direct.push_pull_roundtrip (40 ms)
//!              pulled bytes differ from pushed
//! [ SKIPPED  ] tcpip.shell_echo
//!              no tcpip device connected
//! [==========] 6 tests ran. (180 ms total)
//! [  PASSED  ] 4 tests.
//! [ SKIPPED  ] 1 test.
//! [  FAILED  ] 1 test, listed below:
//! [  FAILED  ] usb_direct.push_pull_roundtrip
//! ```

use std::time::Duration;

/// Outcome of a single test case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The case passed.
    Passed,
    /// The case failed, with a human-readable reason.
    Failed(String),
    /// The case was skipped (precondition not met), with the reason.
    Skipped(String),
}

/// A finished test case: its `suite.name` and outcome plus how long it took.
#[derive(Debug, Clone)]
pub struct CaseResult {
    /// gtest-style suite label (the channel, e.g. `usb_direct`).
    pub suite: String,
    /// Case name within the suite.
    pub name: String,
    /// What happened.
    pub outcome: Outcome,
    /// Wall-clock duration of the case.
    pub elapsed: Duration,
}

impl CaseResult {
    /// `suite.name`, the gtest identifier for this case.
    #[must_use]
    pub fn full_name(&self) -> String {
        format!("{}.{}", self.suite, self.name)
    }
}

// gtest-style banner tags (fixed 10-char `[ ... ]` columns).
const RUN: &str = "[ RUN      ]";
const OK: &str = "[       OK ]";
const FAILED: &str = "[  FAILED  ]";
const SKIPPED: &str = "[ SKIPPED  ]";
const SEP: &str = "[==========]";
const PASSED: &str = "[  PASSED  ]";

/// Accumulates case results and prints gtest-style progress to stdout.
///
/// The reporter owns all console output for the harness (the library stays a
/// pure `tracing` emitter; this is the CLI's user-facing surface, so `println!`
/// is correct here).
#[derive(Default)]
pub struct Reporter {
    results: Vec<CaseResult>,
}

impl Reporter {
    /// A fresh reporter with no results.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Print the `[ RUN ]` line as a case starts.
    pub fn start_case(full_name: &str) {
        println!("{RUN} {full_name}");
    }

    /// Record + print the result of a finished case.
    pub fn finish_case(&mut self, result: CaseResult) {
        let ms = result.elapsed.as_millis();
        match &result.outcome {
            Outcome::Passed => println!("{OK} {} ({ms} ms)", result.full_name()),
            Outcome::Failed(reason) => {
                println!("{FAILED} {} ({ms} ms)", result.full_name());
                println!("             {reason}");
            }
            Outcome::Skipped(reason) => {
                println!("{SKIPPED} {}", result.full_name());
                println!("             {reason}");
            }
        }
        self.results.push(result);
    }

    /// Print the banner announcing how many tests will run.
    pub fn start_run(total: usize) {
        println!("{SEP} Running {total} test{}.", plural(total));
    }

    /// Print the final gtest-style summary and return `true` iff nothing failed.
    ///
    /// A run with only passed/skipped cases is a success; any `Failed` case
    /// flips the overall result (and the process exit code, via the caller).
    pub fn finish_run(&self, total_elapsed: Duration) -> bool {
        let passed = self.count(|o| matches!(o, Outcome::Passed));
        let skipped = self.count(|o| matches!(o, Outcome::Skipped(_)));
        let failures: Vec<&CaseResult> = self
            .results
            .iter()
            .filter(|r| matches!(r.outcome, Outcome::Failed(_)))
            .collect();

        let ms = total_elapsed.as_millis();
        println!(
            "{SEP} {} test{} ran. ({ms} ms total)",
            self.results.len(),
            plural(self.results.len())
        );
        println!("{PASSED} {passed} test{}.", plural(passed));
        if skipped > 0 {
            println!("{SKIPPED} {skipped} test{}.", plural(skipped));
        }
        if failures.is_empty() {
            return true;
        }
        println!(
            "{FAILED} {} test{}, listed below:",
            failures.len(),
            plural(failures.len())
        );
        for f in &failures {
            println!("{FAILED} {}", f.full_name());
        }
        false
    }

    /// Whether any recorded case failed. (Used by tests; the live path uses the
    /// boolean returned from [`Self::finish_run`].)
    #[cfg(test)]
    #[must_use]
    pub fn any_failed(&self) -> bool {
        self.results
            .iter()
            .any(|r| matches!(r.outcome, Outcome::Failed(_)))
    }

    fn count(&self, pred: impl Fn(&Outcome) -> bool) -> usize {
        self.results.iter().filter(|r| pred(&r.outcome)).count()
    }
}

/// gtest pluralization: "1 test" vs "N tests".
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(suite: &str, name: &str, outcome: Outcome) -> CaseResult {
        CaseResult {
            suite: suite.to_string(),
            name: name.to_string(),
            outcome,
            elapsed: Duration::from_millis(1),
        }
    }

    #[test]
    fn full_name_is_suite_dot_case() {
        let c = case("usb_direct", "shell_echo", Outcome::Passed);
        assert_eq!(c.full_name(), "usb_direct.shell_echo");
    }

    #[test]
    fn finish_run_true_when_no_failures() {
        let mut r = Reporter::new();
        r.finish_case(case("s", "a", Outcome::Passed));
        r.finish_case(case("s", "b", Outcome::Skipped("no device".into())));
        assert!(
            r.finish_run(Duration::from_millis(2)),
            "passed+skipped only must be an overall success"
        );
        assert!(!r.any_failed());
    }

    #[test]
    fn finish_run_false_when_any_failure() {
        let mut r = Reporter::new();
        r.finish_case(case("s", "a", Outcome::Passed));
        r.finish_case(case("s", "b", Outcome::Failed("boom".into())));
        assert!(
            !r.finish_run(Duration::from_millis(2)),
            "a single failure must flip the overall result"
        );
        assert!(r.any_failed());
    }

    #[test]
    fn plural_matches_gtest() {
        assert_eq!(plural(1), "");
        assert_eq!(plural(0), "s");
        assert_eq!(plural(2), "s");
    }
}
