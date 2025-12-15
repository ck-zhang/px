//! Configuration, settings, and per-command context assembly.

pub mod context;
pub mod settings;
pub(crate) mod state_guard;

pub use settings::*;
