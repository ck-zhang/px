use std::{fs, path::Path};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use toml_edit::{DocumentMut, InlineTable, Item, Table};

use crate::{
    attach_autosync_details, install_snapshot, manifest_snapshot_at, outcome_from_output,
    python_context_with_mode, refresh_project_site, CommandContext, EnvGuard,
    EnvironmentSyncReport, ExecutionOutcome, InstallUserError, PythonContext,
};
use px_domain::ManifestEditor;

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
    let guard = if request.frozen || ctx.env_flag_enabled("CI") {
        EnvGuard::Strict
    } else {
        EnvGuard::AutoSync
    };
    let (py_ctx, sync_report) = match python_context_with_mode(ctx, guard) {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
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
        match execute_quality_tool(&env, kind, request, tool, true)? {
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
    allow_autoinstall: bool,
) -> Result<ToolRun> {
    let python_args = tool.python_args(&request.args);
    let envs = match env.py_ctx.base_env(env.env_payload) {
        Ok(envs) => envs,
        Err(err) => {
            return Ok(ToolRun::Outcome(ExecutionOutcome::failure(
                "failed to prepare environment for tool",
                json!({ "error": err.to_string() }),
            )));
        }
    };
    let output = env.ctx.python_runtime().run_command(
        &env.py_ctx.python,
        &python_args,
        &envs,
        &env.py_ctx.project_root,
    )?;
    if output.code == 0 {
        return Ok(ToolRun::Completed(QualityRunRecord::new(
            tool.display_name(),
            &tool.module,
            python_args.clone(),
            output,
        )));
    }

    let combined_output = format!("{}{}", output.stdout, output.stderr);
    if missing_module_error(&combined_output, &tool.module) {
        if allow_autoinstall
            && matches!(env.config.source, QualityConfigSource::Default)
            && tool.is_default_ruff()
        {
            match auto_install_default_fmt_tool(env) {
                Ok(()) => {
                    return execute_quality_tool(env, kind, request, tool, false);
                }
                Err(outcome) => return Ok(ToolRun::Outcome(outcome)),
            }
        }
        let mut details = json!({
            "tool": tool.display_name(),
            "module": tool.module,
            "python_args": python_args,
            "config_source": env.config.source.as_str(),
            "pyproject": env.pyproject.display().to_string(),
            "forwarded_args": &request.args,
        });
        if let Some(requirement) = tool.requirement_spec() {
            details["requirement"] = json!(requirement);
            details["hint"] = json!(format!(
                "Run `px add --group dev {requirement}` to install {} in the project env.",
                tool.display_name()
            ));
        } else {
            details["hint"] = json!(format!(
                "Add `requirement = \"{module}==<version>\"` under [tool.px.{section}] so px can suggest an install command.",
                module = tool.module,
                section = kind.section_name(),
            ));
        }
        let mut outcome = ExecutionOutcome::user_error(
            format!(
                "px {}: module `{}` is not installed",
                kind.section_name(),
                tool.module
            ),
            details,
        );
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

    fn is_default_ruff(&self) -> bool {
        self.module == "ruff" && self.requirement.as_deref() == Some(DEFAULT_RUFF_REQUIREMENT)
    }
}

struct QualityRunRecord {
    name: String,
    module: String,
    python_args: Vec<String>,
    stdout: String,
    stderr: String,
    code: i32,
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

fn auto_install_default_fmt_tool(env: &QualityToolEnv<'_, '_>) -> Result<(), ExecutionOutcome> {
    let pyproject = env.py_ctx.project_root.join("pyproject.toml");
    let requirement = DEFAULT_RUFF_REQUIREMENT.to_string();
    let mut editor = ManifestEditor::open(&pyproject).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read pyproject.toml",
            json!({ "error": err.to_string(), "pyproject": pyproject.display().to_string() }),
        )
    })?;
    let _ = editor.add_specs(&[requirement.clone()]).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to update pyproject.toml",
            json!({ "error": err.to_string(), "pyproject": pyproject.display().to_string() }),
        )
    })?;
    let snapshot = manifest_snapshot_at(&env.py_ctx.project_root).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read project snapshot",
            json!({
                "error": err.to_string(),
                "project_root": env.py_ctx.project_root.display().to_string(),
            }),
        )
    })?;
    let install_outcome = match install_snapshot(env.ctx, &snapshot, false, None) {
        Ok(outcome) => outcome,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Err(ExecutionOutcome::user_error(user.message, user.details)),
            Err(other) => {
                return Err(ExecutionOutcome::failure(
                    "failed to install default formatter",
                    json!({ "error": other.to_string() }),
                ))
            }
        },
    };
    if matches!(install_outcome.state, crate::InstallState::MissingLock) {
        return Err(ExecutionOutcome::failure(
            "px fmt could not refresh px.lock for default formatter",
            json!({ "pyproject": pyproject.display().to_string() }),
        ));
    }
    refresh_project_site(&snapshot, env.ctx).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to update project environment after installing formatter",
            json!({ "error": err.to_string() }),
        )
    })?;
    Ok(())
}

impl QualityRunRecord {
    fn new(name: &str, module: &str, python_args: Vec<String>, output: crate::RunOutput) -> Self {
        Self {
            name: name.to_string(),
            module: module.to_string(),
            python_args,
            stdout: output.stdout,
            stderr: output.stderr,
            code: output.code,
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
        })
    }
}
