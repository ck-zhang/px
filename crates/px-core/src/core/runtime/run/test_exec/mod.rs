//! Test execution + runners (pytest/builtin/script).

use super::*;

mod builtin;
mod driver;
mod env;
mod outcome;
mod pytest;
mod script;
mod stdlib;

pub(super) use driver::run_tests_for_context_cas_native;
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
