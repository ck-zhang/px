use std::path::Path;

use color_eyre::Result;
use px_core::api as px_core;
use px_core::{CommandStatus, StatusPayload};

use crate::style::Style;

use super::{OutputOptions, StatusRenderOptions};

pub(super) fn emit_status_output(
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
