use anyhow::Result;

use crate::python_sys::{detect_interpreter_tags, InterpreterTags};

use super::facade::RuntimeMetadata;

/// Versioned identifier for builder environments.
pub(crate) const BUILDER_VERSION: u32 = 12;

#[derive(Clone, Debug)]
pub(crate) struct BuilderIdentity {
    pub(crate) runtime_abi: String,
    pub(crate) builder_id: String,
}

/// Compute the `(runtime_abi, builder_id)` pair for a Python interpreter path.
pub(crate) fn builder_identity_for_python(python: &str) -> Result<BuilderIdentity> {
    let tags = detect_interpreter_tags(python)?;
    Ok(builder_identity_from_tags(&tags))
}

/// Compute the `(runtime_abi, builder_id)` pair for a runtime description.
pub(crate) fn builder_identity_for_runtime(runtime: &RuntimeMetadata) -> Result<BuilderIdentity> {
    builder_identity_for_python(&runtime.path)
}

/// Derive a runtime ABI tag and builder id from interpreter tags.
pub(crate) fn builder_identity_from_tags(tags: &InterpreterTags) -> BuilderIdentity {
    let runtime_abi = runtime_abi_from_tags(tags);
    let builder_id = format!("{runtime_abi}-v{BUILDER_VERSION}");
    BuilderIdentity {
        runtime_abi,
        builder_id,
    }
}

pub(crate) fn runtime_abi_from_tags(tags: &InterpreterTags) -> String {
    let py = tags
        .python
        .first()
        .cloned()
        .unwrap_or_else(|| "py3".to_string());
    let abi = tags
        .abi
        .first()
        .cloned()
        .unwrap_or_else(|| "abi3".to_string());
    let platform = tags
        .platform
        .first()
        .cloned()
        .unwrap_or_else(|| "any".to_string());
    format!("{py}-{abi}-{platform}")
}
