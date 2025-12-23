//! CLI-facing diagnostics, progress reporting, and outcome shaping.

pub(crate) mod diagnostics;
mod messages;
pub(crate) mod outcome;
pub mod progress;
pub(crate) mod timings;

pub(crate) use messages::*;
