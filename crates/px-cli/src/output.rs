use atty::Stream;
use color_eyre::Result;
use px_core::{
    diag_commands, format_status_message, CommandGroup, CommandInfo, CommandStatus,
    ExecutionOutcome, StatusPayload,
};
use serde_json::Value;
use std::path::Path;

use crate::style::Style;
use crate::traceback;

#[derive(Clone, Copy, Debug)]
pub struct OutputOptions {
    pub quiet: bool,
    pub json: bool,
    pub no_color: bool,
    pub verbose: u8,
    pub debug: bool,
    pub trace: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StatusRenderOptions {
    pub brief: bool,
}

pub fn emit_output(
    opts: &OutputOptions,
    subcommand_json: bool,
    status_opts: Option<StatusRenderOptions>,
    info: CommandInfo,
    outcome: &ExecutionOutcome,
) -> Result<i32> {
    let code = match outcome.status {
        CommandStatus::Ok => 0,
        CommandStatus::UserError => 1,
        CommandStatus::Failure => 2,
    };

    let style = Style::new(opts.no_color, atty::is(Stream::Stdout));

    if info.group == CommandGroup::Status {
        if let Ok(payload) = serde_json::from_value::<StatusPayload>(outcome.details.clone()) {
            let render = status_opts.unwrap_or_default();
            return emit_status_output(opts, &style, render, &payload, code);
        }
    }

    if opts.json || subcommand_json {
        let payload = px_core::to_json_response(info, outcome, code);
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if !opts.quiet {
        if info.group == CommandGroup::Explain && matches!(outcome.status, CommandStatus::Ok) {
            println!("{}", outcome.message);
            return Ok(code);
        }
        if handle_run_output(opts, &style, info, outcome) {
            return Ok(code);
        }
        if let Some(note) = autosync_note_from_details(&outcome.details) {
            let line = format!("px {}: {}", info.name, note);
            println!("{}", style.info(&line));
        }
        let migrate_table = render_migrate_table(&style, info, &outcome.details);
        let reporter_rendered = outcome
            .details
            .as_object()
            .and_then(|map| map.get("reporter_rendered"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if info.group == CommandGroup::Test && reporter_rendered {
            return Ok(code);
        }
        if info.group == CommandGroup::Test
            && outcome.status == CommandStatus::Failure
            && render_test_failure(&style, info, outcome)
        {
            if let Some(table) = migrate_table {
                println!("{table}");
            }
            return Ok(code);
        }
        if let CommandStatus::Ok = outcome.status {
            if is_passthrough(&outcome.details) {
                println!("{}", outcome.message);
            } else {
                let message = px_core::format_status_message(info, &outcome.message);
                println!("{}", style.status(&outcome.status, &message));
                let mut hint_emitted = false;
                if let Some(trace) = traceback_from_details(&style, &outcome.details) {
                    println!("{}", trace.body);
                    if let Some(line) = trace.hint_line {
                        println!("{line}");
                        hint_emitted = true;
                    }
                }
                if !hint_emitted {
                    if let Some(hint) = hint_from_details(&outcome.details) {
                        let hint_line = format!("Tip: {hint}");
                        println!("{}", style.info(&hint_line));
                    }
                }
            }
        } else {
            let trace = traceback_from_details(&style, &outcome.details);
            if info.group == CommandGroup::Test
                && outcome
                    .details
                    .as_object()
                    .and_then(|map| map.get("suppress_cli_frame"))
                    .and_then(Value::as_bool)
                    == Some(true)
            {
                return Ok(code);
            }
            let header = format!("{}  {}", error_code(info), outcome.message);
            println!("{}", style.error_header(&header));
            println!();
            println!("Why:");
            for reason in collect_why_bullets(&outcome.details, &outcome.message) {
                println!("  • {reason}");
            }
            let fixes = collect_fix_bullets(&outcome.details);
            if !fixes.is_empty() {
                println!();
                println!("Fix:");
                for fix in fixes {
                    println!("{}", style.fix_bullet(&format!("  • {fix}")));
                }
            }
            let stdout = output_from_details(&outcome.details, "stdout");
            let stderr = output_from_details(&outcome.details, "stderr");
            if let Some(stdout) = stdout {
                println!();
                println!("stdout:");
                println!("{stdout}");
            }
            if let Some(trace) = trace.as_ref() {
                println!();
                println!("{}", trace.body);
                if let Some(line) = trace.hint_line.as_ref() {
                    println!("{line}");
                }
            } else if let Some(stderr) = stderr {
                if !stderr.trim().is_empty() {
                    println!();
                    println!("stderr:");
                    println!("{stderr}");
                }
            }
        }
        if let Some(table) = migrate_table {
            println!("{table}");
        }
    }

    Ok(code)
}

fn handle_run_output(
    opts: &OutputOptions,
    style: &Style,
    info: CommandInfo,
    outcome: &ExecutionOutcome,
) -> bool {
    let trace = traceback_from_details(style, &outcome.details);
    let stdout = output_from_details(&outcome.details, "stdout");
    let stderr = output_from_details(&outcome.details, "stderr");
    if info.group != CommandGroup::Run {
        return false;
    }
    match outcome.status {
        CommandStatus::Ok => {
            if is_passthrough(&outcome.details) {
                if !outcome.message.is_empty() {
                    println!("{}", outcome.message);
                }
            } else if opts.verbose > 0 || opts.debug || opts.trace {
                let message = format_status_message(info, &outcome.message);
                println!("{}", style.status(&outcome.status, &message));
            }
            true
        }
        CommandStatus::Failure => {
            if let Some(stdout) = stdout {
                if !stdout.trim().is_empty() {
                    println!("{stdout}");
                }
            }
            let mut printed_trace = false;
            if let Some(trace) = trace.as_ref() {
                println!("{}", trace.body);
                if let Some(line) = trace.hint_line.as_ref() {
                    println!("{line}");
                }
                printed_trace = true;
            }
            if let Some(stderr) = stderr {
                if !(stderr.trim().is_empty()
                    || (printed_trace && stderr.contains("Traceback (most recent call last):")))
                {
                    println!("{stderr}");
                }
            }
            let summary = format_run_failure_summary(info, outcome);
            println!("{}", style.status(&CommandStatus::Failure, &summary));
            true
        }
        CommandStatus::UserError => false,
    }
}

fn format_run_failure_summary(info: CommandInfo, outcome: &ExecutionOutcome) -> String {
    let target = outcome
        .details
        .as_object()
        .and_then(|map| map.get("target"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("target");
    let code = outcome
        .details
        .as_object()
        .and_then(|map| map.get("code"))
        .and_then(Value::as_i64);
    let mut summary = format!("px {} {target} failed", info.name);
    if let Some(code) = code {
        summary.push_str(&format!(" (exit code {code})"));
    }
    summary
}

fn emit_status_output(
    opts: &OutputOptions,
    style: &Style,
    render: StatusRenderOptions,
    payload: &StatusPayload,
    code: i32,
) -> Result<i32> {
    if opts.quiet {
        return Ok(code);
    }
    if opts.json {
        println!("{}", serde_json::to_string_pretty(payload)?);
        return Ok(code);
    }
    if render.brief {
        println!("{}", format_status_brief(payload));
    } else {
        for line in format_status_default(style, payload) {
            println!("{line}");
        }
    }
    Ok(code)
}

fn format_status_brief(payload: &StatusPayload) -> String {
    let ident = match payload.context.kind {
        px_core::StatusContextKind::Project => payload
            .context
            .project_name
            .clone()
            .unwrap_or_else(|| "project".to_string()),
        px_core::StatusContextKind::Workspace | px_core::StatusContextKind::WorkspaceMember => {
            let ws = payload
                .context
                .workspace_name
                .clone()
                .or_else(|| {
                    payload.context.workspace_root.as_ref().and_then(|root| {
                        Path::new(root)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .map(str::to_string)
                    })
                })
                .unwrap_or_else(|| "workspace".to_string());
            if payload.context.kind == px_core::StatusContextKind::WorkspaceMember {
                if let Some(member) = &payload.context.member_path {
                    format!("{ws}/{member}")
                } else {
                    ws
                }
            } else {
                ws
            }
        }
        px_core::StatusContextKind::None => "px".to_string(),
    };
    let runtime = payload
        .runtime
        .as_ref()
        .and_then(|rt| rt.version.as_ref())
        .map(|v| format!("Python {v}"))
        .unwrap_or_else(|| "Python unknown".to_string());
    let (state, kind) = if let Some(workspace) = &payload.workspace {
        (
            workspace_state_human(&workspace.state),
            "workspace".to_string(),
        )
    } else if let Some(project) = &payload.project {
        (project_state_human(&project.state), "project".to_string())
    } else {
        ("Unknown".to_string(), "project".to_string())
    };
    let mut line = format!("{ident}: {state} ({kind}, {runtime})");
    if payload.next_action.kind != px_core::NextActionKind::None {
        let mut cmd = payload
            .next_action
            .command
            .clone()
            .unwrap_or_else(|| "px sync".to_string());
        if matches!(
            payload.context.kind,
            px_core::StatusContextKind::Workspace | px_core::StatusContextKind::WorkspaceMember
        ) && payload.next_action.kind == px_core::NextActionKind::SyncWorkspace
        {
            cmd.push_str(" at workspace root");
        }
        line.push_str(&format!(" – run `{cmd}`"));
    }
    line
}

fn format_status_default(style: &Style, payload: &StatusPayload) -> Vec<String> {
    let mut lines = Vec::new();
    let pad = label_width(payload);

    match payload.context.kind {
        px_core::StatusContextKind::Project => {
            let name = payload
                .context
                .project_name
                .clone()
                .unwrap_or_else(|| "project".to_string());
            let root = payload.context.project_root.as_deref().unwrap_or_default();
            lines.push(kv(
                "project:",
                &format!("{name}  ({})", style.dim(root)),
                pad,
            ));
            if let Some(runtime) = &payload.runtime {
                lines.push(kv("runtime:", &runtime_line(runtime, payload), pad));
            }
            lines.push(String::new());
            if let Some(project) = &payload.project {
                let ok = matches!(project.state.as_str(), "Consistent" | "InitializedEmpty");
                lines.push(kv(
                    "state:",
                    &state_line(style, ok, &project_state_human(&project.state)),
                    pad,
                ));
                for bullet in project_bullets(project, payload.env.as_ref()) {
                    lines.push(format!("  • {bullet}"));
                }
            }
            lines.push(String::new());
            if let Some(env) = &payload.env {
                lines.push(kv("env:", &env_line(env), pad));
            }
            if let Some(lock) = &payload.lock {
                lines.push(kv("lock:", &lock_line(lock), pad));
            }
        }
        px_core::StatusContextKind::Workspace | px_core::StatusContextKind::WorkspaceMember => {
            let ws_name = payload
                .context
                .workspace_name
                .clone()
                .unwrap_or_else(|| "workspace".to_string());
            let ws_root = payload
                .context
                .workspace_root
                .as_deref()
                .unwrap_or_default();
            lines.push(kv(
                "workspace:",
                &format!("{ws_name}  ({})", style.dim(ws_root)),
                pad,
            ));
            if let Some(member) = &payload.context.member_path {
                lines.push(kv("member:", member, pad));
            }
            lines.push(String::new());
            if let Some(workspace) = &payload.workspace {
                let ws_ok = matches!(
                    workspace.state.as_str(),
                    "WConsistent" | "WInitializedEmpty"
                );
                lines.push(kv(
                    "workspace state:",
                    &state_line(style, ws_ok, &workspace_state_human(&workspace.state)),
                    pad,
                ));
                for bullet in workspace_bullets(workspace, payload.env.as_ref()) {
                    lines.push(format!("  • {bullet}"));
                }
                if payload.context.kind == px_core::StatusContextKind::WorkspaceMember {
                    let included = workspace
                        .members
                        .iter()
                        .find(|member| Some(&member.path) == payload.context.member_path.as_ref())
                        .map(|member| member.manifest_status == "ok")
                        .unwrap_or(false);
                    let text = if included {
                        "Included in workspace manifest"
                    } else {
                        "Not part of workspace manifest"
                    };
                    lines.push(kv(
                        "project state:",
                        &state_line(style, included, text),
                        pad,
                    ));
                }
                if payload.context.kind == px_core::StatusContextKind::Workspace {
                    if !workspace.members.is_empty() {
                        lines.push(String::new());
                        lines.push(kv("members:", "", pad));
                        let member_pad = workspace
                            .members
                            .iter()
                            .map(|m| m.path.len())
                            .max()
                            .unwrap_or(0);
                        for member in &workspace.members {
                            let status = if member.manifest_status == "ok" {
                                "included, clean".to_string()
                            } else {
                                format!("error: {}", member.manifest_status.replace('_', " "))
                            };
                            lines.push(format!(
                                "  • {:<width$}  ({status})",
                                member.path,
                                width = member_pad
                            ));
                        }
                    }
                    if payload.context.member_path.is_none() {
                        lines.push(String::new());
                        lines.push(kv(
                            "note:",
                            "Current directory is not part of any workspace member.",
                            pad,
                        ));
                        lines.push(format!(
                            "{:pad$}  • run `px status` at a member path, or",
                            "",
                            pad = pad
                        ));
                        lines.push(format!(
                            "{:pad$}  • add this directory as a member in [tool.px.workspace].members.",
                            "",
                            pad = pad
                        ));
                    }
                }
            }
            lines.push(String::new());
            if let Some(runtime) = &payload.runtime {
                lines.push(kv("runtime:", &runtime_line(runtime, payload), pad));
            }
            if let Some(env) = &payload.env {
                lines.push(kv("env:", &env_line(env), pad));
            }
            if let Some(lock) = &payload.lock {
                lines.push(kv("lock:", &lock_line(lock), pad));
            }
        }
        px_core::StatusContextKind::None => {
            lines.push("No px project found.".to_string());
        }
    }

    if !payload.warnings.is_empty() {
        lines.push(String::new());
        for warning in &payload.warnings {
            lines.push(kv("warning:", warning, pad));
        }
    }

    if payload.next_action.kind != px_core::NextActionKind::None {
        lines.push(String::new());
        lines.push(kv("next:", &next_line(payload), pad));
    }

    lines
}

fn label_width(payload: &StatusPayload) -> usize {
    let mut labels = vec!["runtime:", "env:", "lock:", "next:", "warning:"];
    match payload.context.kind {
        px_core::StatusContextKind::Project => {
            labels.extend(["project:", "state:"]);
        }
        px_core::StatusContextKind::WorkspaceMember => {
            labels.extend([
                "workspace:",
                "member:",
                "workspace state:",
                "project state:",
            ]);
        }
        px_core::StatusContextKind::Workspace => {
            labels.extend(["workspace:", "workspace state:", "members:", "note:"]);
        }
        px_core::StatusContextKind::None => {}
    }
    labels.iter().map(|l| l.len()).max().unwrap_or(0) + 1
}

fn kv(label: &str, value: &str, width: usize) -> String {
    format!("{label:<width$}{value}")
}

fn state_line(style: &Style, ok: bool, text: &str) -> String {
    let status = if ok {
        CommandStatus::Ok
    } else {
        CommandStatus::UserError
    };
    style.status(&status, text)
}

fn runtime_line(runtime: &px_core::RuntimeStatus, payload: &StatusPayload) -> String {
    let version = runtime.version.as_deref().unwrap_or("unknown").to_string();
    let source = match runtime.source {
        px_core::RuntimeSource::PxManaged => "px-managed",
        px_core::RuntimeSource::System => "system",
        px_core::RuntimeSource::Unknown => "unknown",
    };
    let suffix = match payload.context.kind {
        px_core::StatusContextKind::Workspace | px_core::StatusContextKind::WorkspaceMember => {
            "; workspace"
        }
        _ => "",
    };
    format!("Python {version} ({source}{suffix})")
}

fn env_line(env: &px_core::EnvStatus) -> String {
    let path = env.path.as_deref().unwrap_or("(none)");
    let date = env
        .last_built_at
        .as_deref()
        .and_then(|ts| ts.split('T').next());
    let tag = match env.status {
        px_core::EnvHealth::Clean => date
            .map(|d| format!(" (last built: {d})"))
            .unwrap_or_default(),
        px_core::EnvHealth::Stale => " (stale)".to_string(),
        px_core::EnvHealth::Missing => " (missing)".to_string(),
        px_core::EnvHealth::Unknown => " (unknown)".to_string(),
    };
    format!("{path}{tag}")
}

fn lock_line(lock: &px_core::LockStatus) -> String {
    let path = lock.file.as_deref().unwrap_or("(none)");
    let date = lock
        .updated_at
        .as_deref()
        .and_then(|ts| ts.split('T').next());
    let tag = match lock.status {
        px_core::LockHealth::Clean => date
            .map(|d| format!(" (updated: {d})"))
            .unwrap_or_else(|| " (clean)".to_string()),
        px_core::LockHealth::Mismatch => " (mismatch)".to_string(),
        px_core::LockHealth::Missing => " (missing)".to_string(),
        px_core::LockHealth::Unknown => " (unknown)".to_string(),
    };
    format!("{path}{tag}")
}

fn project_state_human(state: &str) -> String {
    match state {
        "Consistent" => "Consistent",
        "InitializedEmpty" => "Consistent",
        "NeedsLock" => "Needs lock",
        "NeedsEnv" => "Needs environment",
        "Uninitialized" => "Uninitialized",
        other => other,
    }
    .to_string()
}

fn workspace_state_human(state: &str) -> String {
    match state {
        "WConsistent" => "Consistent",
        "WInitializedEmpty" => "Consistent",
        "WNeedsLock" => "Needs workspace lock",
        "WNeedsEnv" => "Needs workspace environment",
        "WUninitialized" => "Uninitialized",
        other => other,
    }
    .to_string()
}

fn project_bullets(
    project: &px_core::ProjectStatusPayload,
    env: Option<&px_core::EnvStatus>,
) -> Vec<String> {
    let mut bullets = Vec::new();
    match project.state.as_str() {
        "Consistent" | "InitializedEmpty" => {
            bullets.push("manifest matches px.lock".to_string());
            bullets.push("environment matches px.lock".to_string());
        }
        "NeedsLock" => {
            if !project.lock_exists {
                bullets.push("px.lock missing".to_string());
            } else {
                bullets.push(
                    "pyproject.toml dependencies changed since px.lock was created".to_string(),
                );
            }
            bullets.push(env_issue_line(env));
        }
        "NeedsEnv" => {
            bullets.push("px.lock is up to date with pyproject.toml".to_string());
            bullets.push(env_issue_line(env));
        }
        _ => {
            bullets.push("project state is unknown".to_string());
        }
    }
    bullets
}

fn workspace_bullets(
    workspace: &px_core::WorkspaceStatusPayload,
    env: Option<&px_core::EnvStatus>,
) -> Vec<String> {
    let mut bullets = Vec::new();
    let has_member_errors = workspace
        .members
        .iter()
        .any(|member| member.manifest_status != "ok");
    match workspace.state.as_str() {
        "WConsistent" | "WInitializedEmpty" => {
            bullets.push("px.workspace.lock matches the workspace manifest".to_string());
            bullets.push("workspace environment matches px.workspace.lock".to_string());
        }
        "WNeedsLock" => {
            if !workspace.lock_exists {
                bullets.push("px.workspace.lock missing".to_string());
            } else if has_member_errors {
                bullets.push("one or more workspace members cannot be parsed".to_string());
            } else {
                bullets.push(
                    "member manifests changed since px.workspace.lock was created".to_string(),
                );
            }
            bullets.push(workspace_env_issue_line(env));
        }
        "WNeedsEnv" => {
            bullets.push("px.workspace.lock matches the workspace manifest".to_string());
            bullets.push(workspace_env_issue_line(env));
        }
        _ => bullets.push("workspace state is unknown".to_string()),
    }
    bullets
}

fn env_issue_line(env: Option<&px_core::EnvStatus>) -> String {
    match env.map(|e| e.status) {
        Some(px_core::EnvHealth::Missing) => {
            "environment missing or not built from px.lock".to_string()
        }
        Some(px_core::EnvHealth::Stale) => "environment no longer matches px.lock".to_string(),
        Some(px_core::EnvHealth::Clean) => "environment matches px.lock".to_string(),
        _ => "environment status is unknown".to_string(),
    }
}

fn workspace_env_issue_line(env: Option<&px_core::EnvStatus>) -> String {
    match env.map(|e| e.status) {
        Some(px_core::EnvHealth::Missing) => {
            "workspace environment missing or out of date".to_string()
        }
        Some(px_core::EnvHealth::Stale) => "workspace environment is out of date".to_string(),
        Some(px_core::EnvHealth::Clean) => {
            "workspace environment matches px.workspace.lock".to_string()
        }
        _ => "workspace environment status is unknown".to_string(),
    }
}

fn next_line(payload: &StatusPayload) -> String {
    match payload.next_action.kind {
        px_core::NextActionKind::Sync => {
            "Run `px sync` to update px.lock and rebuild the environment.".to_string()
        }
        px_core::NextActionKind::SyncWorkspace => {
            "Run `px sync` at the workspace root to update px.workspace.lock and the workspace environment.".to_string()
        }
        px_core::NextActionKind::Init => {
            "Run `px init` to start a new px project here.".to_string()
        }
        px_core::NextActionKind::Migrate => {
            "Run `px migrate` to create px metadata.".to_string()
        }
        px_core::NextActionKind::None => String::new(),
    }
}

fn hint_from_details(details: &Value) -> Option<&str> {
    details
        .as_object()
        .and_then(|map| map.get("hint"))
        .and_then(Value::as_str)
}

fn output_from_details<'a>(details: &'a Value, key: &str) -> Option<&'a str> {
    details
        .as_object()
        .and_then(|map| map.get(key))
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

fn traceback_from_details(style: &Style, details: &Value) -> Option<traceback::TracebackDisplay> {
    let map = details.as_object()?;
    let traceback_value = map.get("traceback")?;
    traceback::format_traceback(style, traceback_value)
}

fn autosync_note_from_details(details: &Value) -> Option<&str> {
    details
        .as_object()
        .and_then(|map| map.get("autosync"))
        .and_then(Value::as_object)
        .and_then(|map| map.get("note"))
        .and_then(Value::as_str)
}

fn is_passthrough(details: &Value) -> bool {
    details
        .as_object()
        .and_then(|map| map.get("passthrough"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn render_migrate_table(style: &Style, info: CommandInfo, details: &Value) -> Option<String> {
    if info.group != CommandGroup::Migrate {
        return None;
    }
    let packages = details.get("packages")?.as_array()?;
    if packages.is_empty() {
        return None;
    }

    let mut rows = Vec::new();
    for pkg in packages {
        let obj = pkg.as_object()?;
        rows.push(PackageRow {
            name: obj.get("name")?.as_str()?.to_string(),
            source: obj.get("source")?.as_str()?.to_string(),
            requested: obj.get("requested")?.as_str()?.to_string(),
            scope: obj.get("scope")?.as_str()?.to_string(),
        });
    }

    Some(format_package_table(style, &rows))
}

struct PackageRow {
    name: String,
    source: String,
    requested: String,
    scope: String,
}

fn format_package_table(style: &Style, rows: &[PackageRow]) -> String {
    let headers = ["Package", "Source", "Requested", "Scope"];
    let mut widths = [
        headers[0].len(),
        headers[1].len(),
        headers[2].len(),
        headers[3].len(),
    ];

    for row in rows {
        widths[0] = widths[0].max(row.name.len());
        widths[1] = widths[1].max(row.source.len());
        widths[2] = widths[2].max(row.requested.len());
        widths[3] = widths[3].max(row.scope.len());
    }

    let header_line = format!(
        "{:<width0$}  {:<width1$}  {:<width2$}  {:<width3$}",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        width0 = widths[0],
        width1 = widths[1],
        width2 = widths[2],
        width3 = widths[3],
    );

    let mut lines = Vec::new();
    lines.push(style.table_header(&header_line));
    lines.push(format!(
        "{:-<width0$}  {:-<width1$}  {:-<width2$}  {:-<width3$}",
        "",
        "",
        "",
        "",
        width0 = widths[0],
        width1 = widths[1],
        width2 = widths[2],
        width3 = widths[3],
    ));

    for row in rows {
        lines.push(format!(
            "{:<width0$}  {:<width1$}  {:<width2$}  {:<width3$}",
            row.name,
            row.source,
            row.requested,
            row.scope,
            width0 = widths[0],
            width1 = widths[1],
            width2 = widths[2],
            width3 = widths[3],
        ));
    }

    lines.join("\n")
}

fn render_test_failure(style: &Style, info: CommandInfo, outcome: &ExecutionOutcome) -> bool {
    let reason = outcome
        .details
        .as_object()
        .and_then(|map| map.get("reason"))
        .and_then(Value::as_str);
    if reason != Some("tests_failed") {
        return false;
    }
    if outcome
        .details
        .as_object()
        .and_then(|map| map.get("suppress_cli_frame"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return true;
    }
    let message = px_core::format_status_message(info, &outcome.message);
    println!("{}", style.status(&outcome.status, &message));
    if let Some(hint) = hint_from_details(&outcome.details) {
        println!("{}", style.info(&format!("Hint: {hint}")));
    }
    true
}

fn error_code(info: CommandInfo) -> &'static str {
    match info.group {
        CommandGroup::Init => diag_commands::INIT,
        CommandGroup::Add => diag_commands::ADD,
        CommandGroup::Remove => diag_commands::REMOVE,
        CommandGroup::Sync => diag_commands::SYNC,
        CommandGroup::Update => diag_commands::UPDATE,
        CommandGroup::Status => diag_commands::STATUS,
        CommandGroup::Run => diag_commands::RUN,
        CommandGroup::Explain => diag_commands::RUN,
        CommandGroup::Test => diag_commands::TEST,
        CommandGroup::Fmt => diag_commands::FMT,
        CommandGroup::Build => diag_commands::BUILD,
        CommandGroup::Publish => diag_commands::PUBLISH,
        CommandGroup::Pack => diag_commands::PACK,
        CommandGroup::Migrate => diag_commands::MIGRATE,
        CommandGroup::Why => diag_commands::WHY,
        CommandGroup::Tool => diag_commands::TOOL,
        CommandGroup::Python => diag_commands::PYTHON,
        CommandGroup::Completions => diag_commands::GENERIC,
    }
}

fn collect_why_bullets(details: &Value, fallback: &str) -> Vec<String> {
    use std::collections::HashSet;

    let mut bullets = Vec::new();
    let mut seen_messages = HashSet::new();
    if let Some(reason) = details.get("reason").and_then(Value::as_str) {
        push_unique(
            &mut bullets,
            reason_display(reason).unwrap_or(reason).to_string(),
        );
    }
    if let Some(status) = details.get("status").and_then(Value::as_str) {
        push_unique(&mut bullets, format!("Status: {status}"));
    }
    if let Some(issues) = details.get("issues").and_then(Value::as_array) {
        for entry in issues {
            match entry {
                Value::String(message) => {
                    if seen_messages.insert(message.clone()) {
                        push_unique(&mut bullets, message.to_string())
                    }
                }
                Value::Object(map) => {
                    let message = map
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if message.is_empty() {
                        continue;
                    }
                    if !seen_messages.insert(message.to_string()) {
                        continue;
                    }
                    if let Some(id) = map.get("id").and_then(Value::as_str) {
                        push_unique(&mut bullets, format!("{id}: {message}"));
                    } else {
                        push_unique(&mut bullets, message.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(drift) = details.get("drift").and_then(Value::as_array) {
        if !drift.is_empty() {
            push_unique(
                &mut bullets,
                format!("Manifest drift detected ({} entries)", drift.len()),
            );
        }
    }
    if bullets.is_empty() {
        bullets.push(fallback.to_string());
    }
    bullets
}

fn collect_fix_bullets(details: &Value) -> Vec<String> {
    let mut fixes = Vec::new();
    if let Some(hint) = hint_from_details(details) {
        push_unique(&mut fixes, hint.to_string());
    }
    if let Some(rec) = details
        .as_object()
        .and_then(|map| map.get("recommendation"))
        .and_then(Value::as_object)
    {
        if let Some(command) = rec.get("command").and_then(Value::as_str) {
            push_unique(&mut fixes, format!("Run `{command}`"));
        }
        if let Some(hint) = rec.get("hint").and_then(Value::as_str) {
            push_unique(&mut fixes, hint.to_string());
        }
    }
    if fixes.is_empty() {
        fixes.push("Re-run with --help for usage or inspect the output above.".to_string());
    }
    fixes
}

fn push_unique(vec: &mut Vec<String>, text: impl Into<String>) {
    let entry = text.into();
    if entry.trim().is_empty() {
        return;
    }
    if !vec.iter().any(|existing| existing == &entry) {
        vec.push(entry);
    }
}

fn reason_display(code: &str) -> Option<&'static str> {
    match code {
        "resolve_no_match" => Some("No compatible release satisfied the requested constraint."),
        "invalid_requirement" => Some("One of the requirements is invalid (PEP 508 parse failed)."),
        "pypi_unreachable" => Some("Unable to reach PyPI while resolving dependencies."),
        "resolve_failed" => Some("Dependency resolver failed."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collect_fix_bullets_orders_hint_and_recommendation() {
        let details = json!({
            "hint": "first-hint",
            "recommendation": {
                "command": "px do-thing",
                "hint": "second-hint"
            }
        });
        let fixes = collect_fix_bullets(&details);
        assert!(
            fixes.iter().any(|f| f == "first-hint"),
            "expected primary hint to be present"
        );
        assert!(
            fixes.iter().any(|f| f == "Run `px do-thing`"),
            "expected recommended command"
        );
        assert!(
            fixes.iter().any(|f| f == "second-hint"),
            "expected secondary hint"
        );
    }

    #[test]
    fn collect_why_bullets_dedupes_and_uses_reason_display() {
        let details = json!({
            "reason": "resolve_no_match",
            "issues": [
                { "id": "E1", "message": "inconsistent spec" },
                { "message": "inconsistent spec" }
            ],
            "status": "pending"
        });
        let bullets = collect_why_bullets(&details, "fallback");
        assert!(
            bullets.iter().any(|b| b.contains("No compatible release")),
            "expected reason to be mapped"
        );
        assert!(
            bullets.iter().any(|b| b.contains("Status: pending")),
            "expected status bullet"
        );
        assert_eq!(
            bullets
                .iter()
                .filter(|b| b.contains("inconsistent spec"))
                .count(),
            1,
            "duplicate issues should be collapsed"
        );
    }
}
