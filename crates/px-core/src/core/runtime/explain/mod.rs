// Mapping note: the former `runtime/explain.rs` mega-module was split into focused files:
// - `entrypoint.rs`: console-script resolution for `px explain entrypoint`
// - `run.rs`: run-plan explanation for `px explain run`
// - `render.rs`: human-readable plan rendering helpers

mod entrypoint;
mod render;
mod run;

pub use entrypoint::explain_entrypoint;
pub use run::explain_run;
