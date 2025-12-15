//! CLI-facing diagnostics, progress reporting, and outcome shaping.

pub(crate) mod diagnostics;
pub(crate) mod messages;
pub(crate) mod outcome;
pub mod progress;

pub(crate) use messages::*;
