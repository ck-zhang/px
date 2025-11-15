use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use px_lockfile::{analyze_lock_diff, load_lockfile_optional, lock_prefetch_specs};
use px_store::{
    prefetch_artifacts, PrefetchOptions as StorePrefetchOptions,
    PrefetchSummary as StorePrefetchSummary,
};
use serde::Serialize;
use serde_json::{json, Value};
use toml_edit::{DocumentMut, Item};

use crate::{
    ensure_pyproject_exists, install_snapshot, manifest_snapshot_at, store_prefetch_specs,
    CommandContext, ExecutionOutcome, InstallState, InstallUserError, ManifestSnapshot,
};

pub(crate) fn prefetch(ctx: &CommandContext, dry_run: bool) -> Result<ExecutionOutcome> {
    let workspace = read_workspace_definition(ctx)?;
    if workspace.members.is_empty() {
        return Ok(workspace_missing_members_outcome(&workspace));
    }

    let cache = ctx.cache();
    let mut totals = StorePrefetchSummary::default();
    let mut members = Vec::new();
    let mut had_error = false;

    for member in &workspace.members {
        let lockfile = member.abs_path.join("px.lock").display().to_string();
        let mut status = "ok".to_string();
        let mut error = None;
        let mut summary = StorePrefetchSummary::default();

        if !member.exists {
            status = "missing-manifest".to_string();
            error = Some(format!(
                "manifest not found at {}",
                member.manifest_path.display()
            ));
            had_error = true;
        } else {
            match manifest_snapshot_at(&member.abs_path) {
                Ok(snapshot) => match load_lockfile_optional(&snapshot.lock_path)? {
                    Some(lock) => match lock_prefetch_specs(&lock) {
                        Ok(specs) => {
                            if specs.is_empty() {
                                status = "missing-artifacts".to_string();
                                error = Some("px.lock does not contain artifact metadata".to_string());
                                had_error = true;
                            } else {
                                let store_specs = store_prefetch_specs(&specs);
                                match prefetch_artifacts(
                                    &cache.path,
                                    &store_specs,
                                    StorePrefetchOptions {
                                        dry_run,
                                        parallel: 4,
                                    },
                                ) {
                                    Ok(result) => {
                                        summary = result;
                                        if summary.failed > 0 {
                                            status = "prefetch-error".to_string();
                                            error = summary.errors.first().cloned();
                                            had_error = true;
                                        }
                                    }
                                    Err(err) => {
                                        status = "prefetch-error".to_string();
                                        summary.requested = store_specs.len();
                                        summary.failed = store_specs.len();
                                        summary.errors.push(err.to_string());
                                        error = Some(err.to_string());
                                        had_error = true;
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            status = "lock-error".to_string();
                            error = Some(err.to_string());
                            had_error = true;
                        }
                    },
                    None => {
                        status = "missing-lock".to_string();
                        error = Some("px.lock not found (run `px install`)".to_string());
                        had_error = true;
                    }
                },
                Err(err) => {
                    status = "manifest-error".to_string();
                    error = Some(err.to_string());
                    had_error = true;
                }
            }
        }

        accumulate_prefetch_summary(&mut totals, &summary);
        members.push(PrefetchWorkspaceMember {
            name: member.name.clone(),
            path: member.rel_path.clone(),
            lockfile: Some(lockfile),
            status,
            summary: summary.clone(),
            error,
        });
    }

    let message = if dry_run {
        format!(
            "workspace dry-run {} artifacts ({} cached)",
            totals.requested, totals.hit
        )
    } else {
        format!(
            "workspace hydrated {} artifacts ({} cached, {} fetched)",
            totals.requested, totals.hit, totals.fetched
        )
    };

    let mut details = json!({
        "cache": {
            "path": cache.path.display().to_string(),
            "source": cache.source,
        },
        "dry_run": dry_run,
        "workspace": {
            "root": workspace.root.display().to_string(),
            "members": members,
            "totals": totals,
        }
    });

    details["status"] = Value::String(if dry_run { "dry-run" } else { "prefetched" }.to_string());

    if had_error {
        Ok(ExecutionOutcome::user_error(
            "workspace prefetch encountered errors",
            details,
        ))
    } else {
        Ok(ExecutionOutcome::success(message, details))
    }
}

pub(crate) fn list(ctx: &CommandContext) -> Result<ExecutionOutcome> {
    let workspace = read_workspace_definition(ctx)?;
    if workspace.members.is_empty() {
        return Ok(workspace_missing_members_outcome(&workspace));
    }

    let details = json!({
        "workspace": {
            "root": workspace.root.display().to_string(),
            "members": workspace
                .members
                .iter()
                .map(|member| json!({
                    "name": member.name,
                    "path": member.rel_path,
                    "manifest": member.manifest_path.display().to_string(),
                    "manifest_exists": member.exists,
                    "lock_exists": member.lock_exists,
                }))
                .collect::<Vec<_>>(),
        },
    });

    let names = workspace
        .members
        .iter()
        .map(|m| m.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Ok(ExecutionOutcome::success(
        format!("workspace members: {names}"),
        details,
    ))
}

pub(crate) fn verify(ctx: &CommandContext) -> Result<ExecutionOutcome> {
    let workspace = read_workspace_definition(ctx)?;
    if workspace.members.is_empty() {
        return Ok(workspace_missing_members_outcome(&workspace));
    }

    let mut member_reports = Vec::new();
    let mut has_drift = false;
    let mut first_issue: Option<(String, String)> = None;

    for member in &workspace.members {
        if !member.exists {
            has_drift = true;
            member_reports.push(json!({
                "name": member.name,
                "path": member.rel_path,
                "status": "missing-manifest",
                "message": format!("manifest not found at {}", member.manifest_path.display()),
            }));
            if first_issue.is_none() {
                first_issue = Some((member.name.clone(), "missing-manifest".to_string()));
            }
            continue;
        }

        let snapshot = match manifest_snapshot_at(&member.abs_path) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                has_drift = true;
                member_reports.push(json!({
                    "name": member.name,
                    "path": member.rel_path,
                    "status": "manifest-error",
                    "message": err.to_string(),
                }));
                if first_issue.is_none() {
                    first_issue = Some((member.name.clone(), "manifest-error".to_string()));
                }
                continue;
            }
        };

        match load_lockfile_optional(&snapshot.lock_path)? {
            Some(lock) => {
                let report = analyze_lock_diff(&snapshot, &lock, None);
                if report.is_clean() {
                    member_reports.push(json!({
                        "name": member.name,
                        "path": member.rel_path,
                        "status": "ok",
                        "lockfile": snapshot.lock_path.display().to_string(),
                    }));
                } else {
                    has_drift = true;
                    member_reports.push(json!({
                        "name": member.name,
                        "path": member.rel_path,
                        "status": "drift",
                        "lockfile": snapshot.lock_path.display().to_string(),
                        "drift": report.to_messages(),
                    }));
                    if first_issue.is_none() {
                        first_issue = Some((member.name.clone(), "drift".to_string()));
                    }
                }
            }
            None => {
                has_drift = true;
                member_reports.push(json!({
                    "name": member.name,
                    "path": member.rel_path,
                    "status": "missing-lock",
                    "lockfile": snapshot.lock_path.display().to_string(),
                }));
                if first_issue.is_none() {
                    first_issue = Some((member.name.clone(), "missing-lock".to_string()));
                }
            }
        }
    }

    let mut details = json!({
        "status": if has_drift { "drift" } else { "clean" },
        "workspace": {
            "root": workspace.root.display().to_string(),
            "members": member_reports,
        }
    });

    if has_drift {
        details["hint"] = Value::String(
            "run `px workspace install` or `px install` inside drifted members".to_string(),
        );
        let summary = summarize_workspace_issue(first_issue);
        Ok(ExecutionOutcome::user_error(summary, details))
    } else {
        Ok(ExecutionOutcome::success("all members clean", details))
    }
}

pub(crate) fn install(ctx: &CommandContext, frozen: bool) -> Result<ExecutionOutcome> {
    let workspace = read_workspace_definition(ctx)?;
    if workspace.members.is_empty() {
        return Ok(workspace_missing_members_outcome(&workspace));
    }

    let mut reports = Vec::new();
    let mut stats = WorkspaceStats::default();

    for member in &workspace.members {
        let mut report = WorkspaceMemberReport::new(member);
        if !member.exists {
            report = report
                .with_status(WorkspaceMemberStatus::MissingManifest)
                .error(format!(
                    "manifest not found at {}",
                    member.manifest_path.display()
                ));
            stats.update(&report.status);
            reports.push(report);
            continue;
        }

        let snapshot = match manifest_snapshot_at(&member.abs_path) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                report = report
                    .with_status(WorkspaceMemberStatus::ManifestError)
                    .error(err.to_string());
                stats.update(&report.status);
                reports.push(report);
                continue;
            }
        };

        match install_snapshot(ctx, &snapshot, frozen, None) {
            Ok(result) => {
                report = report.lockfile(result.lockfile.clone());
                report = match result.state {
                    InstallState::Installed => report.with_status(WorkspaceMemberStatus::Installed),
                    InstallState::UpToDate => {
                        if frozen && result.verified {
                            report.with_status(WorkspaceMemberStatus::Verified)
                        } else {
                            report.with_status(WorkspaceMemberStatus::UpToDate)
                        }
                    }
                    InstallState::Drift => report
                        .with_status(WorkspaceMemberStatus::Drift)
                        .drift(result.drift),
                    InstallState::MissingLock => {
                        report.with_status(WorkspaceMemberStatus::MissingLock)
                    }
                };
            }
            Err(err) => match err.downcast::<InstallUserError>() {
                Ok(user) => {
                    report = report
                        .with_status(WorkspaceMemberStatus::InstallError)
                        .error(user.message);
                }
                Err(err) => {
                    report = report
                        .with_status(WorkspaceMemberStatus::InstallError)
                        .error(err.to_string());
                }
            },
        }

        stats.update(&report.status);
        reports.push(report);
    }

    finalize_workspace_outcome(
        if frozen {
            "workspace install --frozen"
        } else {
            "workspace install"
        },
        workspace,
        reports,
        stats,
    )
}

pub(crate) fn tidy(ctx: &CommandContext) -> Result<ExecutionOutcome> {
    let workspace = read_workspace_definition(ctx)?;
    if workspace.members.is_empty() {
        return Ok(workspace_missing_members_outcome(&workspace));
    }

    let mut reports = Vec::new();
    let mut stats = WorkspaceStats::default();

    for member in &workspace.members {
        let mut report = WorkspaceMemberReport::new(member);
        if !member.exists {
            report = report
                .with_status(WorkspaceMemberStatus::MissingManifest)
                .error(format!(
                    "manifest not found at {}",
                    member.manifest_path.display()
                ));
            stats.update(&report.status);
            reports.push(report);
            continue;
        }

        let snapshot = match manifest_snapshot_at(&member.abs_path) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                report = report
                    .with_status(WorkspaceMemberStatus::ManifestError)
                    .error(err.to_string());
                stats.update(&report.status);
                reports.push(report);
                continue;
            }
        };

        match tidy_snapshot(&snapshot)? {
            TidyOutcome {
                state: TidyState::Clean,
                lockfile,
                ..
            } => {
                report = report
                    .with_status(WorkspaceMemberStatus::Tidied)
                    .lockfile(lockfile);
            }
            TidyOutcome {
                state: TidyState::Drift,
                lockfile,
                drift,
            } => {
                report = report
                    .with_status(WorkspaceMemberStatus::Drift)
                    .lockfile(lockfile)
                    .drift(drift);
            }
            TidyOutcome {
                state: TidyState::MissingLock,
                lockfile,
                ..
            } => {
                report = report
                    .with_status(WorkspaceMemberStatus::MissingLock)
                    .lockfile(lockfile);
            }
        }

        stats.update(&report.status);
        reports.push(report);
    }

    finalize_workspace_outcome("workspace tidy", workspace, reports, stats)
}

fn workspace_missing_members_outcome(workspace: &WorkspaceDefinition) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        "no [tool.px.workspace] members declared",
        json!({
            "workspace": {
                "root": workspace.root.display().to_string(),
                "members": Vec::<Value>::new(),
            },
            "hint": "add [tool.px.workspace].members entries in pyproject.toml",
        }),
    )
}

fn finalize_workspace_outcome(
    label: &str,
    workspace: WorkspaceDefinition,
    reports: Vec<WorkspaceMemberReport>,
    stats: WorkspaceStats,
) -> Result<ExecutionOutcome> {
    let total = reports.len();
    let details = workspace_details(&workspace, &reports, &stats);
    let summary = workspace_summary(label, &stats, total);
    if stats.has_error() {
        Ok(ExecutionOutcome::user_error(summary, details))
    } else {
        Ok(ExecutionOutcome::success(summary, details))
    }
}

fn workspace_details(
    workspace: &WorkspaceDefinition,
    reports: &[WorkspaceMemberReport],
    stats: &WorkspaceStats,
) -> Value {
    json!({
        "workspace": {
            "root": workspace.root.display().to_string(),
            "counts": stats.counts_value(reports.len()),
            "members": reports.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
        }
    })
}

fn workspace_summary(_label: &str, stats: &WorkspaceStats, total: usize) -> String {
    if stats.has_error() {
        format!(
            "{}/{} clean, {} drifted, {} failed",
            stats.ok, total, stats.drifted, stats.failed
        )
    } else {
        format!("all {total} members clean")
    }
}

fn read_workspace_definition(ctx: &CommandContext) -> Result<WorkspaceDefinition> {
    let root = ctx.project_root()?;
    let manifest_path = root.join("pyproject.toml");
    ensure_pyproject_exists(&manifest_path)?;
    let contents = fs::read_to_string(&manifest_path)?;
    let doc: DocumentMut = contents.parse()?;

    let members_item = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table)
        .and_then(|px| px.get("workspace"))
        .and_then(Item::as_table)
        .and_then(|workspace| workspace.get("members"));

    let mut members = Vec::new();
    if let Some(item) = members_item {
        if let Some(array) = item.as_array() {
            for value in array.iter() {
                if let Some(rel) = value.as_str() {
                    let rel_path = rel.to_string();
                    let abs_path = root.join(rel);
                    let member_manifest = abs_path.join("pyproject.toml");
                    let exists = member_manifest.exists();
                    let name = if exists {
                        discover_project_name(&member_manifest).unwrap_or_else(|| rel_path.clone())
                    } else {
                        rel_path.clone()
                    };
                    let lock_exists = abs_path.join("px.lock").exists();
                    members.push(WorkspaceMember {
                        name,
                        rel_path,
                        abs_path,
                        manifest_path: member_manifest,
                        exists,
                        lock_exists,
                    });
                }
            }
        }
    }

    Ok(WorkspaceDefinition { root, members })
}

fn discover_project_name(manifest_path: &Path) -> Option<String> {
    let contents = fs::read_to_string(manifest_path).ok()?;
    let doc: DocumentMut = contents.parse().ok()?;
    doc.get("project")
        .and_then(Item::as_table)
        .and_then(|table| table.get("name"))
        .and_then(Item::as_str)
        .map(|s| s.to_string())
}

fn summarize_workspace_issue(issue: Option<(String, String)>) -> String {
    if let Some((name, status)) = issue {
        match status.as_str() {
            "missing-manifest" => format!("member {name} missing manifest"),
            "manifest-error" => format!("member {name} manifest error"),
            "missing-lock" => format!("drift in {name} (px.lock missing)"),
            "drift" => format!("drift in {name} (lock mismatch)"),
            other => format!("drift in {name} ({other})"),
        }
    } else {
        "workspace drift detected".to_string()
    }
}

#[derive(Clone)]
struct WorkspaceDefinition {
    root: PathBuf,
    members: Vec<WorkspaceMember>,
}

#[derive(Clone)]
struct WorkspaceMember {
    name: String,
    rel_path: String,
    abs_path: PathBuf,
    manifest_path: PathBuf,
    exists: bool,
    lock_exists: bool,
}

enum WorkspaceMemberStatus {
    Installed,
    UpToDate,
    Verified,
    Tidied,
    Drift,
    MissingLock,
    MissingManifest,
    ManifestError,
    InstallError,
}

impl WorkspaceMemberStatus {
    fn as_str(&self) -> &'static str {
        match self {
            WorkspaceMemberStatus::Installed => "installed",
            WorkspaceMemberStatus::UpToDate => "up-to-date",
            WorkspaceMemberStatus::Verified => "verified",
            WorkspaceMemberStatus::Tidied => "tidied",
            WorkspaceMemberStatus::Drift => "drift",
            WorkspaceMemberStatus::MissingLock => "missing-lock",
            WorkspaceMemberStatus::MissingManifest => "missing-manifest",
            WorkspaceMemberStatus::ManifestError => "manifest-error",
            WorkspaceMemberStatus::InstallError => "install-error",
        }
    }

    fn is_ok(&self) -> bool {
        matches!(
            self,
            WorkspaceMemberStatus::Installed
                | WorkspaceMemberStatus::UpToDate
                | WorkspaceMemberStatus::Verified
                | WorkspaceMemberStatus::Tidied
        )
    }

    fn is_drift(&self) -> bool {
        matches!(
            self,
            WorkspaceMemberStatus::Drift | WorkspaceMemberStatus::MissingLock
        )
    }
}

struct WorkspaceMemberReport {
    name: String,
    path: String,
    status: WorkspaceMemberStatus,
    lockfile: Option<String>,
    drift: Vec<String>,
    error: Option<String>,
}

impl WorkspaceMemberReport {
    fn new(member: &WorkspaceMember) -> Self {
        Self {
            name: member.name.clone(),
            path: member.rel_path.clone(),
            status: WorkspaceMemberStatus::UpToDate,
            lockfile: None,
            drift: Vec::new(),
            error: None,
        }
    }

    fn with_status(mut self, status: WorkspaceMemberStatus) -> Self {
        self.status = status;
        self
    }

    fn lockfile(mut self, path: impl Into<String>) -> Self {
        self.lockfile = Some(path.into());
        self
    }

    fn drift(mut self, drift: Vec<String>) -> Self {
        self.drift = drift;
        self
    }

    fn error(mut self, err: impl Into<String>) -> Self {
        self.error = Some(err.into());
        self
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "path": self.path,
            "status": self.status.as_str(),
            "lockfile": self.lockfile,
            "drift": self.drift,
            "error": self.error,
        })
    }
}

#[derive(Serialize)]
struct PrefetchWorkspaceMember {
    name: String,
    path: String,
    lockfile: Option<String>,
    status: String,
    summary: StorePrefetchSummary,
    error: Option<String>,
}

fn accumulate_prefetch_summary(target: &mut StorePrefetchSummary, addition: &StorePrefetchSummary) {
    target.requested += addition.requested;
    target.hit += addition.hit;
    target.fetched += addition.fetched;
    target.failed += addition.failed;
    target.bytes_fetched += addition.bytes_fetched;
    if !addition.errors.is_empty() {
        target.errors.extend(addition.errors.iter().cloned());
    }
}

#[derive(Default)]
struct WorkspaceStats {
    ok: usize,
    drifted: usize,
    failed: usize,
}

impl WorkspaceStats {
    fn update(&mut self, status: &WorkspaceMemberStatus) {
        if status.is_ok() {
            self.ok += 1;
        } else if status.is_drift() {
            self.drifted += 1;
        } else {
            self.failed += 1;
        }
    }

    fn has_error(&self) -> bool {
        self.drifted > 0 || self.failed > 0
    }

    fn counts_value(&self, total: usize) -> Value {
        json!({
            "total": total,
            "ok": self.ok,
            "drifted": self.drifted,
            "failed": self.failed,
        })
    }
}

struct TidyOutcome {
    state: TidyState,
    lockfile: String,
    drift: Vec<String>,
}

enum TidyState {
    Clean,
    Drift,
    MissingLock,
}

fn tidy_snapshot(snapshot: &ManifestSnapshot) -> Result<TidyOutcome> {
    let lockfile = snapshot.lock_path.display().to_string();
    match load_lockfile_optional(&snapshot.lock_path)? {
        Some(lock) => {
            let report = analyze_lock_diff(snapshot, &lock, None);
            if report.is_clean() {
                Ok(TidyOutcome {
                    state: TidyState::Clean,
                    lockfile,
                    drift: Vec::new(),
                })
            } else {
                Ok(TidyOutcome {
                    state: TidyState::Drift,
                    lockfile,
                    drift: report.to_messages(),
                })
            }
        }
        None => Ok(TidyOutcome {
            state: TidyState::MissingLock,
            lockfile,
            drift: Vec::new(),
        }),
    }
}
