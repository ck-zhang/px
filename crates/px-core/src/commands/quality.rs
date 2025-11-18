use std::{fs, path::Path};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use toml_edit::{DocumentMut, InlineTable, Item, Table};

use crate::{
    attach_autosync_details, detect_lock_drift, load_lockfile_optional, manifest_snapshot,
    outcome_from_output, python_context_with_mode, CommandContext, CommandStatus, EnvGuard,
    ExecutionOutcome, ProjectAddRequest,
};

const DEFAULT_RUFF_REQUIREMENT: &str = "ruff==0.6.9";

#[derive(Clone, Debug, Default)]
pub struct QualityTidyRequest;

#[derive(Clone, Debug)]
pub struct ToolCommandRequest {
    pub args: Vec<String>,
}

pub fn quality_tidy(
    _ctx: &CommandContext,
    _request: QualityTidyRequest,
) -> Result<ExecutionOutcome> {
    quality_tidy_outcome()
}

pub fn quality_fmt(ctx: &CommandContext, request: ToolCommandRequest) -> Result<ExecutionOutcome> {
    run_quality_command(ctx, QualityKind::Fmt, &request)
}

pub fn quality_lint(ctx: &CommandContext, request: ToolCommandRequest) -> Result<ExecutionOutcome> {
    run_quality_command(ctx, QualityKind::Lint, &request)
}

fn quality_tidy_outcome() -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;

    let lock = match load_lockfile_optional(&snapshot.lock_path)? {
        Some(lock) => lock,
        None => {
            return Ok(ExecutionOutcome::user_error(
                "px tidy: px.lock not found (run `px sync`)",
                json!({
                    "lockfile": snapshot.lock_path.display().to_string(),
                    "hint": "run `px sync` to generate px.lock before running tidy",
                }),
            ))
        }
    };

    let drift = detect_lock_drift(&snapshot, &lock, None);
    if drift.is_empty() {
        Ok(ExecutionOutcome::success(
            "px.lock matches pyproject",
            json!({
                "status": "clean",
                "lockfile": snapshot.lock_path.display().to_string(),
            }),
        ))
    } else {
        Ok(ExecutionOutcome::user_error(
            "px.lock is out of date",
            json!({
                "status": "drift",
                "lockfile": snapshot.lock_path.display().to_string(),
                "drift": drift,
                "hint": "rerun `px sync` to refresh the lockfile",
            }),
        ))
    }
}

fn run_quality_command(
    ctx: &CommandContext,
    kind: QualityKind,
    request: &ToolCommandRequest,
) -> Result<ExecutionOutcome> {
    let strict = ctx.env_flag_enabled("CI");
    let guard = if strict {
        EnvGuard::Strict
    } else {
        EnvGuard::AutoSync
    };
    let (mut py_ctx, mut sync_report) = match python_context_with_mode(ctx, guard) {
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
            return Ok(ExecutionOutcome::user_error(
                format!("px {}: invalid tool configuration", kind.section_name()),
                json!({
                    "pyproject": pyproject.display().to_string(),
                    "section": format!("[tool.px.{}]", kind.section_name()),
                    "error": err.to_string(),
                }),
            ));
        }
    };
    if config.tools.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            format!("px {}: no tools configured", kind.section_name()),
            json!({
                "pyproject": pyproject.display().to_string(),
                "section": format!("[tool.px.{}]", kind.section_name()),
                "hint": "Add tool definitions or rely on the default Ruff runner",
            }),
        ));
    }

    let env_payload = json!({
        "tool": kind.section_name(),
        "forwarded_args": &request.args,
        "config_source": config.source.as_str(),
    });
    let mut installed_tools: Vec<String> = Vec::new();
    let mut runs = Vec::new();
    for tool in &config.tools {
        let python_args = tool.python_args(&request.args);
        let mut attempts = 0;
        loop {
            attempts += 1;
            let envs = match py_ctx.base_env(&env_payload) {
                Ok(envs) => envs,
                Err(err) => {
                    return Ok(ExecutionOutcome::failure(
                        "failed to prepare environment for tool",
                        json!({ "error": err.to_string() }),
                    ))
                }
            };
            let output = ctx.python_runtime().run_command(
                &py_ctx.python,
                &python_args,
                &envs,
                &py_ctx.project_root,
            )?;
            if output.code == 0 {
                runs.push(QualityRunRecord::new(
                    tool.display_name(),
                    &tool.module,
                    python_args.clone(),
                    output,
                ));
                break;
            }

            let combined_output = format!("{}{}", output.stdout, output.stderr);
            if attempts == 1 && missing_module_error(&combined_output, &tool.module) {
                let Some(requirement) = tool.requirement_spec() else {
                    let mut outcome = ExecutionOutcome::user_error(
                        format!(
                            "px {}: module `{}` is not installed",
                            kind.section_name(),
                            tool.module
                        ),
                        json!({
                            "tool": tool.display_name(),
                            "module": tool.module,
                            "pyproject": pyproject.display().to_string(),
                            "hint": format!(
                                "Add `requirement = \"{module}==<version>\"` under [tool.px.{section}] so px can provision it automatically",
                                module = tool.module,
                                section = kind.section_name(),
                            ),
                        }),
                    );
                    attach_autosync_details(&mut outcome, sync_report);
                    return Ok(outcome);
                };
                match ensure_tool_dependency(ctx, &requirement)? {
                    ToolDependencyResult::Satisfied => {
                        installed_tools.push(requirement);
                        match python_context_with_mode(ctx, guard) {
                            Ok((new_ctx, new_report)) => {
                                if sync_report.is_none() {
                                    sync_report = new_report;
                                }
                                py_ctx = new_ctx;
                                continue;
                            }
                            Err(outcome) => return Ok(outcome),
                        }
                    }
                    ToolDependencyResult::Outcome(mut outcome) => {
                        attach_autosync_details(&mut outcome, sync_report);
                        return Ok(outcome);
                    }
                }
            }

            let mut failure = outcome_from_output(
                kind.section_name(),
                tool.display_name(),
                output,
                &format!("px {}", kind.section_name()),
                Some(json!({
                    "tool": tool.display_name(),
                    "module": tool.module,
                    "python_args": python_args,
                    "config_source": config.source.as_str(),
                    "pyproject": pyproject.display().to_string(),
                    "forwarded_args": &request.args,
                })),
            );
            attach_autosync_details(&mut failure, sync_report);
            return Ok(failure);
        }
    }

    let mut details = json!({
        "runs": runs.iter().map(|run| run.to_json()).collect::<Vec<_>>(),
        "pyproject": pyproject.display().to_string(),
        "config_source": config.source.as_str(),
    });
    if !request.args.is_empty() {
        details["forwarded_args"] = json!(&request.args);
    }
    if !installed_tools.is_empty() {
        details["tools_installed"] = json!(installed_tools);
    }
    if config.source == QualityConfigSource::Default {
        details["hint"] = json!(
            "Configure [tool.px.".to_owned()
                + kind.section_name()
                + "] to override the default Ruff runner"
        );
    }

    let mut outcome = ExecutionOutcome::success(kind.success_message(), details);
    attach_autosync_details(&mut outcome, sync_report);
    Ok(outcome)
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
            .map(|s| s.to_string());
        let requirement = table
            .get("requirement")
            .and_then(Item::as_str)
            .map(|s| s.to_string());
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
        for entry in array.iter() {
            let inline = entry
                .as_inline_table()
                .ok_or_else(|| anyhow!("commands entries must be inline tables"))?;
            tools.push(QualityTool::from_inline_table(inline)?);
        }
        return Ok(tools);
    }
    if let Some(array) = item.as_array_of_tables() {
        let mut tools = Vec::new();
        for table in array.iter() {
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
    for value in array.iter() {
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
    Lint,
}

impl QualityKind {
    fn section_name(&self) -> &'static str {
        match self {
            QualityKind::Fmt => "fmt",
            QualityKind::Lint => "lint",
        }
    }

    fn success_message(&self) -> &'static str {
        match self {
            QualityKind::Fmt => "formatted source files",
            QualityKind::Lint => "linted source files",
        }
    }

    fn default_tools(&self) -> Vec<QualityTool> {
        match self {
            QualityKind::Fmt => vec![QualityTool::new(
                "ruff".to_string(),
                vec!["format".to_string()],
                None,
                Some(DEFAULT_RUFF_REQUIREMENT.to_string()),
            )],
            QualityKind::Lint => vec![QualityTool::new(
                "ruff".to_string(),
                vec!["check".to_string()],
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
            .map(|s| s.to_string());
        let requirement = table
            .get("requirement")
            .and_then(Item::as_str)
            .map(|s| s.to_string());
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
}

struct QualityRunRecord {
    name: String,
    module: String,
    python_args: Vec<String>,
    stdout: String,
    stderr: String,
    code: i32,
}

fn missing_module_error(output: &str, module: &str) -> bool {
    let needle = format!("No module named '{module}'");
    let needle_unquoted = format!("No module named {module}");
    output.contains(&needle) || output.contains(&needle_unquoted)
}

enum ToolDependencyResult {
    Satisfied,
    Outcome(ExecutionOutcome),
}

fn ensure_tool_dependency(ctx: &CommandContext, spec: &str) -> Result<ToolDependencyResult> {
    if spec.trim().is_empty() {
        return Ok(ToolDependencyResult::Satisfied);
    }
    let request = ProjectAddRequest {
        specs: vec![spec.to_string()],
    };
    match super::project::project_add(ctx, request) {
        Ok(outcome) => {
            if matches!(outcome.status, CommandStatus::Ok) {
                Ok(ToolDependencyResult::Satisfied)
            } else {
                Ok(ToolDependencyResult::Outcome(outcome))
            }
        }
        Err(err) => Err(err),
    }
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
