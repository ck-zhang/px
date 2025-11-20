use std::{env, fs, path::Path};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use toml_edit::{DocumentMut, InlineTable, Item, Table};

use crate::{
    attach_autosync_details, auto_sync_environment, build_pythonpath,
    ensure_project_environment_synced, issue_from_details, manifest_snapshot, outcome_from_output,
    python_context_with_mode, runtime_manager,
    tools::{disable_proxy_env, load_installed_tool, MIN_PYTHON_REQUIREMENT},
    CommandContext, EnvGuard, EnvironmentSyncReport, ExecutionOutcome, InstallUserError,
    PythonContext,
};
use px_domain::ProjectSnapshot;

const DEFAULT_RUFF_REQUIREMENT: &str = "ruff==0.6.9";

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
    let guard = EnvGuard::Strict;
    let snapshot = manifest_snapshot()?;
    let (py_ctx, sync_report) = match python_context_with_mode(ctx, guard) {
        Ok(result) => result,
        Err(outcome) => {
            let Some(issue) = issue_from_details(&outcome.details) else {
                return Ok(outcome);
            };
            if !matches!(
                issue,
                crate::EnvironmentIssue::MissingEnv | crate::EnvironmentIssue::MissingArtifacts
            ) {
                return Ok(outcome);
            }
            match auto_sync_environment(ctx, &snapshot, issue) {
                Ok(report) => match python_context_with_mode(ctx, guard) {
                    Ok((py_ctx, inner_report)) => {
                        let merged = inner_report.or(report);
                        (py_ctx, merged)
                    }
                    Err(outcome) => return Ok(outcome),
                },
                Err(err) => {
                    return Ok(ExecutionOutcome::failure(
                        "failed to prepare formatter environment",
                        json!({ "error": err.to_string() }),
                    ))
                }
            }
        }
    };
    let pyproject = py_ctx.project_root.join("pyproject.toml");
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
        py_ctx: &py_ctx,
        config: &config,
        env_payload: &env_payload,
        pyproject: &pyproject,
        sync_report: &sync_report,
    };
    let mut runs = Vec::new();
    for tool in &config.tools {
        match execute_quality_tool(&env, kind, request, tool)? {
            ToolRun::Completed(record) => runs.push(record),
            ToolRun::Outcome(outcome) => return Ok(outcome),
        }
    }

    let details = build_success_details(&runs, &pyproject, &config, kind, request);
    let mut outcome = ExecutionOutcome::success(kind.success_message(), details);
    attach_autosync_details(&mut outcome, sync_report);
    Ok(outcome)
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
        Err(mut outcome) => {
            attach_autosync_details(&mut outcome, env.sync_report.clone());
            return Ok(ToolRun::Outcome(outcome));
        }
    };
    let output = env.ctx.python_runtime().run_command(
        &prepared.python,
        &python_args,
        &prepared.envs,
        &env.py_ctx.project_root,
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
        let mut outcome = missing_module_outcome(env, kind, tool, &python_args, &prepared, request);
        attach_autosync_details(&mut outcome, env.sync_report.clone());
        return Ok(ToolRun::Outcome(outcome));
    }

    let mut failure = outcome_from_output(
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
    attach_autosync_details(&mut failure, env.sync_report.clone());
    Ok(ToolRun::Outcome(failure))
}

struct QualityToolEnv<'ctx, 'payload> {
    ctx: &'ctx CommandContext<'ctx>,
    py_ctx: &'ctx PythonContext,
    config: &'ctx QualityToolConfig,
    env_payload: &'payload Value,
    pyproject: &'ctx Path,
    sync_report: &'ctx Option<EnvironmentSyncReport>,
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
    let descriptor = match load_installed_tool(tool.install_name()) {
        Ok(desc) => desc,
        Err(user) => {
            let mut details = json!({
                "tool": tool.display_name(),
                "module": tool.module,
                "config_source": env.config.source.as_str(),
                "hint": tool.install_command(),
            });
            if !user.details.is_null() {
                details["tool_state"] = user.details;
            }
            return Err(ExecutionOutcome::user_error(
                format!(
                    "px {}: tool '{}' is not installed",
                    kind.section_name(),
                    tool.display_name()
                ),
                details,
            ));
        }
    };
    let runtime_selection = match runtime_manager::resolve_runtime(
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
    };

    let snapshot = ProjectSnapshot::read_from(&descriptor.root).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read tool metadata",
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

    let (pythonpath, mut allowed_paths) = build_pythonpath(env.ctx.fs(), &descriptor.root)
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to prepare formatter environment variables",
                json!({
                    "tool": descriptor.name,
                    "error": err.to_string(),
                }),
            )
        })?;
    let project_src = env.py_ctx.project_root.join("src");
    if project_src.exists() {
        allowed_paths.push(project_src);
    }
    allowed_paths.push(env.py_ctx.project_root.clone());
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
        ("PYTHONPATH".into(), pythonpath),
        ("PYTHONUNBUFFERED".into(), "1".into()),
        ("PX_ALLOWED_PATHS".into(), allowed),
        ("PX_TOOL_ROOT".into(), descriptor.root.display().to_string()),
        (
            "PX_PROJECT_ROOT".into(),
            env.py_ctx.project_root.display().to_string(),
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

fn load_quality_tools(pyproject: &Path, kind: QualityKind) -> Result<QualityToolConfig> {
    let contents = fs::read_to_string(pyproject)
        .with_context(|| format!("reading {}", pyproject.display()))?;
    let doc: DocumentMut = contents.parse()?;
    let section_table = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table)
        .and_then(|px| px.get(kind.section_name()))
        .and_then(Item::as_table);

    if let Some(table) = section_table {
        let tools = parse_quality_section(table)?;
        if tools.is_empty() {
            return Err(anyhow!(
                "no commands configured under [tool.px.{}]",
                kind.section_name()
            ));
        }
        return Ok(QualityToolConfig {
            tools,
            source: QualityConfigSource::Pyproject,
        });
    }

    Ok(QualityToolConfig {
        tools: kind.default_tools(),
        source: QualityConfigSource::Default,
    })
}

fn parse_quality_section(table: &Table) -> Result<Vec<QualityTool>> {
    if let Some(item) = table.get("commands") {
        let tools = parse_commands_item(item)?;
        if !tools.is_empty() {
            return Ok(tools);
        }
    }

    if let Some(module) = table.get("module").and_then(Item::as_str) {
        let args = parse_item_string_array(table.get("args"))?;
        let label = table
            .get("label")
            .and_then(Item::as_str)
            .map(ToString::to_string);
        let requirement = table
            .get("requirement")
            .and_then(Item::as_str)
            .map(ToString::to_string);
        return Ok(vec![QualityTool::new(
            module.to_string(),
            args,
            label,
            requirement,
        )]);
    }
    Ok(Vec::new())
}

fn parse_commands_item(item: &Item) -> Result<Vec<QualityTool>> {
    if let Some(array) = item.as_array() {
        let mut tools = Vec::new();
        for entry in array {
            let inline = entry
                .as_inline_table()
                .ok_or_else(|| anyhow!("commands entries must be inline tables"))?;
            tools.push(QualityTool::from_inline_table(inline)?);
        }
        return Ok(tools);
    }
    if let Some(array) = item.as_array_of_tables() {
        let mut tools = Vec::new();
        for table in array {
            tools.push(QualityTool::from_table(table)?);
        }
        return Ok(tools);
    }
    Ok(Vec::new())
}

fn parse_item_string_array(item: Option<&Item>) -> Result<Vec<String>> {
    let Some(array_item) = item else {
        return Ok(Vec::new());
    };
    let array = array_item
        .as_array()
        .ok_or_else(|| anyhow!("args must be an array of strings"))?;
    let mut values = Vec::new();
    for value in array {
        let literal = value
            .as_str()
            .ok_or_else(|| anyhow!("args entries must be strings"))?;
        values.push(literal.to_string());
    }
    Ok(values)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QualityKind {
    Fmt,
}

impl QualityKind {
    fn section_name(self) -> &'static str {
        match self {
            QualityKind::Fmt => "fmt",
        }
    }

    fn success_message(self) -> &'static str {
        match self {
            QualityKind::Fmt => "formatted source files",
        }
    }

    fn default_tools(self) -> Vec<QualityTool> {
        match self {
            QualityKind::Fmt => vec![QualityTool::new(
                "ruff".to_string(),
                vec!["format".to_string()],
                None,
                Some(DEFAULT_RUFF_REQUIREMENT.to_string()),
            )],
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum QualityConfigSource {
    Default,
    Pyproject,
}

impl QualityConfigSource {
    fn as_str(&self) -> &'static str {
        match self {
            QualityConfigSource::Default => "default",
            QualityConfigSource::Pyproject => "pyproject",
        }
    }
}

struct QualityToolConfig {
    tools: Vec<QualityTool>,
    source: QualityConfigSource,
}

#[derive(Clone)]
struct QualityTool {
    label: String,
    module: String,
    args: Vec<String>,
    requirement: Option<String>,
}

impl QualityTool {
    fn new(
        module: String,
        args: Vec<String>,
        label: Option<String>,
        requirement: Option<String>,
    ) -> Self {
        let label = label.unwrap_or_else(|| module.clone());
        Self {
            label,
            module,
            args,
            requirement,
        }
    }

    fn from_table(table: &Table) -> Result<Self> {
        let module = table
            .get("module")
            .and_then(Item::as_str)
            .ok_or_else(|| anyhow!("tool entry missing `module`"))?
            .to_string();
        let args = parse_item_string_array(table.get("args"))?;
        let label = table
            .get("label")
            .and_then(Item::as_str)
            .map(ToString::to_string);
        let requirement = table
            .get("requirement")
            .and_then(Item::as_str)
            .map(ToString::to_string);
        Ok(Self::new(module, args, label, requirement))
    }

    fn from_inline_table(inline: &InlineTable) -> Result<Self> {
        let table = inline.clone().into_table();
        Self::from_table(&table)
    }

    fn display_name(&self) -> &str {
        &self.label
    }

    fn python_args(&self, forwarded: &[String]) -> Vec<String> {
        let mut args = Vec::with_capacity(2 + self.args.len() + forwarded.len());
        args.push("-m".to_string());
        args.push(self.module.clone());
        args.extend(self.args.iter().cloned());
        args.extend(forwarded.iter().cloned());
        args
    }

    fn requirement_spec(&self) -> Option<String> {
        self.requirement.clone()
    }

    fn install_name(&self) -> &str {
        &self.module
    }

    fn install_command(&self) -> String {
        match self.requirement_spec() {
            Some(requirement) => format!("px tool install {requirement}"),
            None => format!("px tool install {}", self.module),
        }
    }
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

fn missing_module_error(output: &str, module: &str) -> bool {
    let needle = format!("No module named '{module}'");
    let needle_unquoted = format!("No module named {module}");
    output.contains(&needle) || output.contains(&needle_unquoted)
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
