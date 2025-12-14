// Test execution + runners (pytest/builtin/script).
// Mapping note: the former `run/test_exec.rs` mega-module was split into focused files:
// - `driver.rs`: test execution orchestration + CAS-native integration
// - `pytest.rs`: pytest runner + plugin wiring
// - `builtin.rs`: fallback builtin runner
// - `script.rs`: project-provided `runtests.py` runner
// - `env.rs`: environment variable helpers for runners
// - `stdlib.rs`: staging stdlib tests for minimal environments
// - `outcome.rs`: `ExecutionOutcome` shaping helpers

use super::*;

mod builtin;
mod driver;
mod env;
mod outcome;
mod pytest;
mod script;
mod stdlib;

pub(super) use driver::{run_tests_for_context, test_project_outcome};

#[cfg(test)]
pub(super) use driver::find_runtests_script;
#[cfg(test)]
pub(super) use env::merged_pythonpath;
#[cfg(test)]
pub(super) use pytest::{
    build_pytest_command, build_pytest_invocation, default_pytest_flags, missing_pytest,
    TestReporter,
};
