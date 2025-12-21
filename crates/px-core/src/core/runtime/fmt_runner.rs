use std::{env, io::IsTerminal, path::Path};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::fmt_plan::{
    load_quality_tools, missing_module_error, QualityConfigSource, QualityKind, QualityTool,
    QualityToolConfig,
};
use crate::{
    build_pythonpath, ensure_project_environment_synced, is_missing_project_error,
    manifest_snapshot, missing_project_outcome, outcome_from_output,
    progress::ProgressSuspendGuard,
    tools::{
        disable_proxy_env, ensure_tool_env_scripts, load_installed_tool, tool_install,
        repair_tool_env_from_lock, ToolInstallRequest, MIN_PYTHON_REQUIREMENT,
    },
    CommandContext, CommandStatus, ExecutionOutcome, InstallUserError,
};
use px_domain::api::ProjectSnapshot;

use super::runtime_manager;

#[derive(Clone, Debug)]
pub struct FmtRequest {
    pub args: Vec<String>,
    pub frozen: bool,
}

/// Runs configured formatters for the current project.
///
/// # Errors
/// Returns an error if the formatting configuration is invalid or any tool invocation fails.
pub fn run_fmt(ctx: &CommandContext, request: &FmtRequest) -> Result<ExecutionOutcome> {
    run_quality_command(ctx, QualityKind::Fmt, request)
}

fn run_quality_command(
    ctx: &CommandContext,
    kind: QualityKind,
    request: &FmtRequest,
) -> Result<ExecutionOutcome> {
    let snapshot = match manifest_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            if is_missing_project_error(&err) {
                return Ok(missing_project_outcome());
            }
            return Err(err);
        }
    };
    let pyproject = snapshot.manifest_path.clone();
    let config = match load_quality_tools(&pyproject, kind) {
        Ok(config) => config,
        Err(err) => {
            if err.downcast_ref::<std::io::Error>().is_some() {
                return Err(err);
            }
            return Ok(invalid_tool_config_outcome(kind, &pyproject, &err));
        }
    };
    if config.tools.is_empty() {
        return Ok(no_tools_configured_outcome(kind, &pyproject));
    }

    let env_payload = json!({
        "tool": kind.section_name(),
        "forwarded_args": &request.args,
        "config_source": config.source.as_str(),
    });
    let env = QualityToolEnv {
        ctx,
        config: &config,
        env_payload: &env_payload,
        pyproject: &pyproject,
        project_root: snapshot.root.clone(),
    };
    let mut runs = Vec::new();
    for tool in &config.tools {
        match execute_quality_tool(&env, kind, request, tool)? {
            ToolRun::Completed(record) => runs.push(record),
            ToolRun::Outcome(outcome) => return Ok(outcome),
        }
    }

    let details = build_success_details(&runs, &pyproject, &config, kind, request);
    Ok(ExecutionOutcome::success(kind.success_message(), details))
}

fn execute_quality_tool(
    env: &QualityToolEnv<'_, '_>,
    kind: QualityKind,
    request: &FmtRequest,
    tool: &QualityTool,
) -> Result<ToolRun> {
    let python_args = tool.python_args(&request.args);
    let prepared = match prepare_tool_run(env, kind, tool) {
        Ok(prepared) => prepared,
        Err(outcome) => return Ok(ToolRun::Outcome(outcome)),
    };
    let output = env.ctx.python_runtime().run_command(
        &prepared.python,
        &python_args,
        &prepared.envs,
        &env.project_root,
    )?;
    if output.code == 0 {
        return Ok(ToolRun::Completed(QualityRunRecord::new(
            tool.display_name(),
            &tool.module,
            python_args.clone(),
            output,
            &prepared.runtime,
            &prepared.tool_root,
        )));
    }

    let combined_output = format!("{}{}", output.stdout, output.stderr);
    if missing_module_error(&combined_output, &tool.module) {
        let outcome = missing_module_outcome(env, kind, tool, &python_args, &prepared, request);
        return Ok(ToolRun::Outcome(outcome));
    }

    let failure = outcome_from_output(
        kind.section_name(),
        tool.display_name(),
        &output,
        &format!("px {}", kind.section_name()),
        Some(json!({
                "tool": tool.display_name(),
            "module": tool.module,
            "python_args": python_args,
            "config_source": env.config.source.as_str(),
            "pyproject": env.pyproject.display().to_string(),
            "forwarded_args": &request.args,
            "tool_root": prepared.tool_root,
            "runtime": prepared.runtime,
        })),
    );
    Ok(ToolRun::Outcome(failure))
}

struct QualityToolEnv<'ctx, 'payload> {
    ctx: &'ctx CommandContext<'ctx>,
    config: &'ctx QualityToolConfig,
    env_payload: &'payload Value,
    pyproject: &'ctx Path,
    project_root: std::path::PathBuf,
}

struct PreparedToolRun {
    python: String,
    envs: Vec<(String, String)>,
    tool_root: String,
    runtime: String,
}

fn prepare_tool_run(
    env: &QualityToolEnv<'_, '_>,
    kind: QualityKind,
    tool: &QualityTool,
) -> Result<PreparedToolRun, ExecutionOutcome> {
    let install_request = ToolInstallRequest {
        name: tool.install_name().to_string(),
        spec: tool.requirement_spec(),
        python: tool_install_python_override(),
        entry: Some(tool.module.clone()),
    };
    let descriptor = match load_installed_tool(tool.install_name()) {
        Ok(desc) => desc,
        Err(_) => {
            if should_announce_tool_install(env.ctx) {
                eprintln!(
                    "px {}: installing {}",
                    kind.section_name(),
                    tool_install_display(tool)
                );
            }
            let _suspend = ProgressSuspendGuard::new();
            let outcome = match tool_install(env.ctx, &install_request) {
                Ok(outcome) => outcome,
                Err(err) => match err.downcast::<InstallUserError>() {
                    Ok(user) => {
                        return Err(ExecutionOutcome::user_error(user.message, user.details));
                    }
                    Err(other) => {
                        return Err(ExecutionOutcome::failure(
                            format!("px {}: failed to install tool", kind.section_name()),
                            json!({
                                "tool": tool.display_name(),
                                "module": tool.module,
                                "error": other.to_string(),
                                "config_source": env.config.source.as_str(),
                            }),
                        ));
                    }
                },
            };
            if outcome.status != CommandStatus::Ok {
                return Err(outcome);
            }
            load_installed_tool(tool.install_name()).map_err(|err| {
                ExecutionOutcome::failure(
                    format!(
                        "px {}: failed to load tool after install",
                        kind.section_name()
                    ),
                    json!({
                        "tool": tool.display_name(),
                        "module": tool.module,
                        "error": err.to_string(),
                        "config_source": env.config.source.as_str(),
                    }),
                )
            })?
        }
    };
    let runtime_selection = if !descriptor.runtime_path.trim().is_empty() {
        runtime_manager::RuntimeSelection {
            record: runtime_manager::RuntimeRecord {
                version: descriptor.runtime_version.clone(),
                full_version: descriptor.runtime_full_version.clone(),
                path: descriptor.runtime_path.clone(),
                default: false,
            },
            source: runtime_manager::RuntimeSource::Explicit,
        }
    } else {
        match runtime_manager::resolve_runtime(
            Some(&descriptor.runtime_version),
            MIN_PYTHON_REQUIREMENT,
        ) {
            Ok(runtime) => runtime,
            Err(err) => {
                return Err(ExecutionOutcome::user_error(
                    format!(
                        "px {}: Python runtime {} for tool '{}' is unavailable",
                        kind.section_name(),
                        descriptor.runtime_version,
                        descriptor.name
                    ),
                    json!({
                        "tool": descriptor.name,
                        "module": tool.module,
                        "runtime": descriptor.runtime_version,
                        "config_source": env.config.source.as_str(),
                        "hint": err.to_string(),
                    }),
                ));
            }
        }
    };

    let mut snapshot = ProjectSnapshot::read_from(&descriptor.root).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read tool metadata",
            json!({
                "tool": descriptor.name,
                "root": descriptor.root.display().to_string(),
                "error": err.to_string(),
            }),
        )
    })?;

    ensure_tool_env_scripts(&descriptor.root).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to prepare tool environment scripts",
            json!({
                "tool": descriptor.name,
                "root": descriptor.root.display().to_string(),
                "error": err.to_string(),
            }),
        )
    })?;

    if let Err(err) = ensure_project_environment_synced(env.ctx, &snapshot) {
        match err.downcast::<InstallUserError>() {
            Ok(user) => {
                let _suspend = ProgressSuspendGuard::new();
                if snapshot.lock_path.exists() {
                    if let Err(err) = repair_tool_env_from_lock(env.ctx, &descriptor.root, &runtime_selection) {
                        return Err(ExecutionOutcome::user_error(
                            format!(
                                "px {}: tool '{}' is not ready",
                                kind.section_name(),
                                descriptor.name
                            ),
                            json!({
                                "tool": descriptor.name,
                                "module": tool.module,
                                "config_source": env.config.source.as_str(),
                                "details": user.details,
                                "error": err.to_string(),
                                "hint": tool.install_command(),
                            }),
                        ));
                    }
                } else {
                    let outcome = match tool_install(env.ctx, &install_request) {
                        Ok(outcome) => outcome,
                        Err(err) => match err.downcast::<InstallUserError>() {
                            Ok(user) => {
                                return Err(ExecutionOutcome::user_error(user.message, user.details));
                            }
                            Err(other) => {
                                return Err(ExecutionOutcome::failure(
                                    format!(
                                        "px {}: failed to repair tool environment",
                                        kind.section_name()
                                    ),
                                    json!({
                                        "tool": descriptor.name,
                                        "module": tool.module,
                                        "error": other.to_string(),
                                        "config_source": env.config.source.as_str(),
                                    }),
                                ));
                            }
                        },
                    };
                    if outcome.status != CommandStatus::Ok {
                        return Err(outcome);
                    }
                    snapshot = ProjectSnapshot::read_from(&descriptor.root).map_err(|err| {
                        ExecutionOutcome::failure(
                            "failed to read tool metadata",
                            json!({
                                "tool": descriptor.name,
                                "root": descriptor.root.display().to_string(),
                                "error": err.to_string(),
                            }),
                        )
                    })?;
                }
                ensure_tool_env_scripts(&descriptor.root).map_err(|err| {
                    ExecutionOutcome::failure(
                        "failed to prepare tool environment scripts",
                        json!({
                            "tool": descriptor.name,
                            "root": descriptor.root.display().to_string(),
                            "error": err.to_string(),
                        }),
                    )
                })?;
                if let Err(err) = ensure_project_environment_synced(env.ctx, &snapshot) {
                    return match err.downcast::<InstallUserError>() {
                        Ok(user) => Err(ExecutionOutcome::user_error(
                            format!(
                                "px {}: tool '{}' is not ready",
                                kind.section_name(),
                                descriptor.name
                            ),
                            json!({
                                "tool": descriptor.name,
                                "module": tool.module,
                                "config_source": env.config.source.as_str(),
                                "details": user.details,
                                "hint": tool.install_command(),
                            }),
                        )),
                        Err(other) => Err(ExecutionOutcome::failure(
                            "failed to prepare tool environment",
                            json!({
                                "tool": descriptor.name,
                                "root": descriptor.root.display().to_string(),
                                "error": other.to_string(),
                            }),
                        )),
                    };
                }
            }
            Err(other) => {
                return Err(ExecutionOutcome::failure(
                    "failed to prepare tool environment",
                    json!({
                        "tool": descriptor.name,
                        "root": descriptor.root.display().to_string(),
                        "error": other.to_string(),
                    }),
                ));
            }
        }
    }

    let paths = build_pythonpath(env.ctx.fs(), &descriptor.root, None).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to prepare formatter environment variables",
            json!({
                "tool": descriptor.name,
                "error": err.to_string(),
            }),
        )
    })?;
    let mut allowed_paths = paths.allowed_paths;
    let project_src = env.project_root.join("src");
    if project_src.exists() {
        allowed_paths.push(project_src);
    }
    allowed_paths.push(env.project_root.clone());
    let allowed = env::join_paths(&allowed_paths)
        .context("allowed path contains invalid UTF-8")
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to assemble formatter allowed paths",
                json!({
                    "tool": descriptor.name,
                    "error": err.to_string(),
                }),
            )
        })?
        .into_string()
        .map_err(|_| {
            ExecutionOutcome::failure(
                "allowed path contains non-utf8 data",
                json!({
                    "tool": descriptor.name,
                }),
            )
        })?;

    let mut envs = vec![
        ("PYTHONPATH".into(), paths.pythonpath),
        ("PYTHONUNBUFFERED".into(), "1".into()),
        ("PX_ALLOWED_PATHS".into(), allowed),
        ("PX_TOOL_ROOT".into(), descriptor.root.display().to_string()),
        (
            "PX_PROJECT_ROOT".into(),
            env.project_root.display().to_string(),
        ),
        ("PX_COMMAND_JSON".into(), env.env_payload.to_string()),
    ];
    disable_proxy_env(&mut envs);

    Ok(PreparedToolRun {
        python: runtime_selection.record.path,
        envs,
        tool_root: descriptor.root.display().to_string(),
        runtime: runtime_selection.record.full_version,
    })
}

fn tool_install_display(tool: &QualityTool) -> String {
    tool.requirement_spec()
        .unwrap_or_else(|| tool.install_name().to_string())
}

fn tool_install_python_override() -> Option<String> {
    if runtime_manager::resolve_runtime(None, MIN_PYTHON_REQUIREMENT).is_ok() {
        return None;
    }
    std::env::var("PX_RUNTIME_PYTHON")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn should_announce_tool_install(ctx: &CommandContext) -> bool {
    !ctx.global.json && !ctx.global.quiet && std::io::stderr().is_terminal()
}

fn missing_module_outcome(
    env: &QualityToolEnv<'_, '_>,
    kind: QualityKind,
    tool: &QualityTool,
    python_args: &[String],
    prepared: &PreparedToolRun,
    request: &FmtRequest,
) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        format!(
            "px {}: module `{}` is not installed in tool environment",
            kind.section_name(),
            tool.module
        ),
        json!({
            "tool": tool.display_name(),
            "module": tool.module,
            "python_args": python_args,
            "config_source": env.config.source.as_str(),
            "pyproject": env.pyproject.display().to_string(),
            "forwarded_args": &request.args,
            "tool_root": &prepared.tool_root,
            "runtime": &prepared.runtime,
            "hint": tool.install_command(),
        }),
    )
}

fn build_success_details(
    runs: &[QualityRunRecord],
    pyproject: &Path,
    config: &QualityToolConfig,
    kind: QualityKind,
    request: &FmtRequest,
) -> Value {
    let mut details = json!({
        "runs": runs.iter().map(QualityRunRecord::to_json).collect::<Vec<_>>(),
        "pyproject": pyproject.display().to_string(),
        "config_source": config.source.as_str(),
    });
    if !runs.is_empty() {
        let tools: Vec<Value> = runs
            .iter()
            .map(|run| {
                json!({
                    "tool": run.name,
                    "runtime": run.runtime,
                    "tool_root": run.tool_root,
                })
            })
            .collect();
        details["tools"] = json!(tools);
    }
    if !request.args.is_empty() {
        details["forwarded_args"] = json!(&request.args);
    }
    if config.source == QualityConfigSource::Default {
        details["hint"] = json!(
            "Configure [tool.px.".to_owned()
                + kind.section_name()
                + "] to override the default Ruff runner"
        );
    }
    details
}

fn invalid_tool_config_outcome(
    kind: QualityKind,
    pyproject: &Path,
    err: &anyhow::Error,
) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        format!("px {}: invalid tool configuration", kind.section_name()),
        json!({
            "pyproject": pyproject.display().to_string(),
            "section": format!("[tool.px.{}]", kind.section_name()),
            "error": err.to_string(),
        }),
    )
}

fn no_tools_configured_outcome(kind: QualityKind, pyproject: &Path) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        format!("px {}: no tools configured", kind.section_name()),
        json!({
            "pyproject": pyproject.display().to_string(),
            "section": format!("[tool.px.{}]", kind.section_name()),
            "hint": "Add tool definitions or rely on the default Ruff runner",
        }),
    )
}

struct QualityRunRecord {
    name: String,
    module: String,
    python_args: Vec<String>,
    stdout: String,
    stderr: String,
    code: i32,
    runtime: String,
    tool_root: String,
}

enum ToolRun {
    Completed(QualityRunRecord),
    Outcome(ExecutionOutcome),
}

impl QualityRunRecord {
    fn new(
        name: &str,
        module: &str,
        python_args: Vec<String>,
        output: crate::RunOutput,
        runtime: &str,
        tool_root: &str,
    ) -> Self {
        Self {
            name: name.to_string(),
            module: module.to_string(),
            python_args,
            stdout: output.stdout,
            stderr: output.stderr,
            code: output.code,
            runtime: runtime.to_string(),
            tool_root: tool_root.to_string(),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "tool": self.name,
            "module": self.module,
            "python_args": self.python_args,
            "stdout": self.stdout,
            "stderr": self.stderr,
            "code": self.code,
            "runtime": self.runtime,
            "tool_root": self.tool_root,
        })
    }
}
