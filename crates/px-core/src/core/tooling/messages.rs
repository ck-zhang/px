use std::path::Path;

use serde_json::json;

use crate::ExecutionOutcome;

pub(crate) fn run_target_required_outcome() -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        "px run requires a target",
        json!({
            "hint": "pass a command or script explicitly (file under the project or an executable on PATH)",
        }),
    )
}

pub(crate) fn missing_pyproject_outcome(command: &str, root: &Path) -> ExecutionOutcome {
    let manifest = root.join("pyproject.toml");
    ExecutionOutcome::user_error(
        format!("pyproject.toml not found in {}", root.display()),
        json!({
            "hint": "run `px migrate --apply` or pass a target explicitly",
            "project_root": root.display().to_string(),
            "manifest": manifest.display().to_string(),
            "command": command,
        }),
    )
}
