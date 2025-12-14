use super::super::execution_plan;

fn sh_quote(raw: &str) -> String {
    if raw.is_empty() {
        return "''".to_string();
    }
    let safe = raw.chars().all(|ch| {
        ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | '@' | '+')
    });
    if safe {
        raw.to_string()
    } else {
        let escaped = raw.replace('\'', "'\"'\"'");
        format!("'{escaped}'")
    }
}

pub(super) fn render_plan_human(plan: &execution_plan::ExecutionPlan, verbose: u8) -> String {
    let mut lines = Vec::new();
    match &plan.provenance.source {
        execution_plan::SourceProvenance::WorkingTree => {
            lines.push("source: working_tree".to_string())
        }
        execution_plan::SourceProvenance::GitRef {
            git_ref,
            manifest_repo_path,
            ..
        } => lines.push(format!("source: git_ref {git_ref} ({manifest_repo_path})")),
        execution_plan::SourceProvenance::RepoSnapshot {
            locator,
            git_ref,
            commit,
            repo_snapshot_oid,
            script_repo_path,
        } => {
            let mut line = match (commit.as_deref(), git_ref.as_deref()) {
                (Some(commit), _) => {
                    format!("source: repo_snapshot {locator}@{commit}:{script_repo_path}")
                }
                (None, Some(git_ref)) => {
                    format!("source: repo_snapshot {locator}@{git_ref}:{script_repo_path}")
                }
                (None, None) => format!("source: repo_snapshot {locator}:{script_repo_path}"),
            };
            if verbose > 0 {
                if let Some(oid) = repo_snapshot_oid.as_deref() {
                    line.push_str(&format!(" (oid={oid})"));
                }
            }
            lines.push(line);
        }
    }
    let mut engine = plan.engine.mode.to_string();
    if verbose > 0 {
        if let Some(code) = plan.engine.fallback_reason_code.as_deref() {
            engine = format!("{engine} (fallback={code})");
        }
    }
    lines.push(format!("engine: {engine}"));
    if let Some(version) = plan.runtime.python_version.as_deref() {
        if let Some(abi) = plan.runtime.python_abi.as_deref() {
            lines.push(format!(
                "runtime: {} (version={version} abi={abi})",
                plan.runtime.executable
            ));
        } else {
            lines.push(format!(
                "runtime: {} (version={version})",
                plan.runtime.executable
            ));
        }
    } else {
        lines.push(format!("runtime: {}", plan.runtime.executable));
    }
    if let Some(profile) = plan.lock_profile.profile_oid.as_deref() {
        lines.push(format!("profile_oid: {profile}"));
    }
    if let Some(lock_id) = plan
        .lock_profile
        .l_id
        .as_deref()
        .or(plan.lock_profile.wl_id.as_deref())
    {
        lines.push(format!("lock_id: {lock_id}"));
    }
    lines.push(format!("workdir: {}", plan.working_dir));
    lines.push(format!(
        "argv: {}",
        plan.target_resolution
            .argv
            .iter()
            .map(|s| sh_quote(s))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    if plan.sys_path.summary.count > 0 {
        lines.push(format!("sys.path: {} entries", plan.sys_path.summary.count));
    }
    if plan.would_repair_env {
        lines.push("would_repair_env: true (run `px sync`)".to_string());
    }
    lines.join("\n")
}
