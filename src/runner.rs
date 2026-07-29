//! Subprocess layer. loom's only external dependency at runtime is `git`
//! itself: leases, locks, and telemetry are git objects and refs, so the
//! transport, auth, and replication story is git's. The [`Runner`] trait
//! keeps every command unit-testable against a scripted mock with no repo
//! and no network.

use std::process::Command;

use crate::error::{Error, Result};

pub trait Runner {
    fn run(&self, program: &str, args: &[&str]) -> Result<String>;

    /// Nonzero exit becomes `Ok(None)` — for probes where failure is an answer.
    fn try_run(&self, program: &str, args: &[&str]) -> Result<Option<String>> {
        match self.run(program, args) {
            Ok(s) => Ok(Some(s)),
            Err(Error::Git { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<String> {
        let output = Command::new(program).args(args).output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound && program == "git" {
                Error::MissingDependency("git")
            } else {
                Error::Io(e)
            }
        })?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string())
        } else {
            Err(Error::Git {
                args: args.iter().map(|s| s.to_string()).collect(),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr)
                    .trim_end()
                    .to_string(),
            })
        }
    }
}

#[cfg(test)]
pub mod mock {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    pub enum Step {
        Ok(&'static str),
        Fail(&'static str),
    }

    pub struct MockRunner {
        script: RefCell<VecDeque<(String, Step)>>,
        pub calls: RefCell<Vec<String>>,
    }

    impl MockRunner {
        pub fn new(steps: Vec<(&str, Step)>) -> Self {
            Self {
                script: RefCell::new(steps.into_iter().map(|(k, v)| (k.to_string(), v)).collect()),
                calls: RefCell::new(Vec::new()),
            }
        }

        pub fn assert_done(&self) {
            assert!(
                self.script.borrow().is_empty(),
                "unconsumed mock steps remain"
            );
        }
    }

    impl Runner for MockRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<String> {
            let call = format!("{} {}", program, args.join(" "));
            self.calls.borrow_mut().push(call.clone());
            let (expected, step) = self
                .script
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected call: {call}"));
            assert!(
                call.contains(&expected),
                "call mismatch:\n  expected fragment: {expected}\n  actual: {call}"
            );
            match step {
                Step::Ok(s) => Ok(s.to_string()),
                Step::Fail(stderr) => Err(Error::Git {
                    args: args.iter().map(|s| s.to_string()).collect(),
                    code: Some(1),
                    stderr: stderr.to_string(),
                }),
            }
        }
    }

    #[test]
    fn try_run_converts_git_failure_to_none() {
        let r = MockRunner::new(vec![("push", Step::Fail("stale info"))]);
        assert_eq!(r.try_run("git", &["push"]).unwrap(), None);
        r.assert_done();
    }
}
