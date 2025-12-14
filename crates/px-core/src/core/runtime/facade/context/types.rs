use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use px_domain::api::PxOptions;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommandGroup {
    Init,
    Add,
    Remove,
    Sync,
    Update,
    Run,
    Test,
    Fmt,
    Explain,
    Build,
    Publish,
    Pack,
    Migrate,
    Status,
    Why,
    Tool,
    Python,
    Completions,
}

impl fmt::Display for CommandGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            CommandGroup::Init => "init",
            CommandGroup::Add => "add",
            CommandGroup::Remove => "remove",
            CommandGroup::Sync => "sync",
            CommandGroup::Update => "update",
            CommandGroup::Run => "run",
            CommandGroup::Test => "test",
            CommandGroup::Fmt => "fmt",
            CommandGroup::Explain => "explain",
            CommandGroup::Build => "build",
            CommandGroup::Publish => "publish",
            CommandGroup::Pack => "pack",
            CommandGroup::Migrate => "migrate",
            CommandGroup::Status => "status",
            CommandGroup::Why => "why",
            CommandGroup::Tool => "tool",
            CommandGroup::Python => "python",
            CommandGroup::Completions => "completions",
        };
        f.write_str(name)
    }
}

pub(crate) struct PythonContext {
    pub(crate) project_root: PathBuf,
    pub(crate) project_name: String,
    pub(crate) python: String,
    pub(crate) pythonpath: String,
    pub(crate) allowed_paths: Vec<PathBuf>,
    pub(crate) site_bin: Option<PathBuf>,
    pub(crate) pep582_bin: Vec<PathBuf>,
    pub(crate) pyc_cache_prefix: Option<PathBuf>,
    pub(crate) px_options: PxOptions,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum EnvGuard {
    Strict,
    AutoSync,
}

#[derive(Clone, Debug)]
pub(crate) struct EnvironmentSyncReport {
    action: &'static str,
    note: String,
}

impl EnvironmentSyncReport {
    pub(crate) fn new(issue: EnvironmentIssue) -> Self {
        Self {
            action: issue.action_key(),
            note: issue.note().to_string(),
        }
    }

    pub(crate) fn action(&self) -> &str {
        self.action
    }

    pub(super) fn to_json(&self) -> Value {
        json!({
            "action": self.action,
            "note": self.note,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum EnvironmentIssue {
    MissingLock,
    LockDrift,
    MissingArtifacts,
    MissingEnv,
    EnvOutdated,
    RuntimeMismatch,
}

impl EnvironmentIssue {
    pub(super) fn from_details(details: &Value) -> Option<Self> {
        let reason = details
            .as_object()
            .and_then(|map| map.get("reason"))
            .and_then(Value::as_str)?;
        match reason {
            "missing_lock" => Some(EnvironmentIssue::MissingLock),
            "lock_drift" => Some(EnvironmentIssue::LockDrift),
            "missing_artifacts" => Some(EnvironmentIssue::MissingArtifacts),
            "missing_env" => Some(EnvironmentIssue::MissingEnv),
            "env_outdated" => Some(EnvironmentIssue::EnvOutdated),
            "runtime_mismatch" => Some(EnvironmentIssue::RuntimeMismatch),
            _ => None,
        }
    }

    fn note(self) -> &'static str {
        self.lock_message().unwrap_or_else(|| self.env_message())
    }

    pub(super) fn lock_message(self) -> Option<&'static str> {
        match self {
            EnvironmentIssue::MissingLock => Some("Updating px.lock (missing lock)"),
            EnvironmentIssue::LockDrift => Some("Updating px.lock (manifest changed)"),
            _ => None,
        }
    }

    pub(super) fn env_message(self) -> &'static str {
        match self {
            EnvironmentIssue::MissingLock | EnvironmentIssue::LockDrift => "Syncing environment…",
            EnvironmentIssue::MissingArtifacts => "Syncing environment (rehydrating cache)…",
            EnvironmentIssue::MissingEnv => "Syncing environment…",
            EnvironmentIssue::EnvOutdated => "Syncing environment…",
            EnvironmentIssue::RuntimeMismatch => "Syncing environment (runtime changed)…",
        }
    }

    pub(super) fn needs_lock_resolution(self) -> bool {
        self.lock_message().is_some()
    }

    fn action_key(self) -> &'static str {
        match self {
            EnvironmentIssue::MissingLock => "lock-bootstrap",
            EnvironmentIssue::LockDrift => "lock-sync",
            EnvironmentIssue::MissingArtifacts => "env-rehydrate",
            EnvironmentIssue::MissingEnv => "env-recreate",
            EnvironmentIssue::EnvOutdated => "env-refresh",
            EnvironmentIssue::RuntimeMismatch => "env-runtime",
        }
    }

    pub(super) fn auto_fixable(self) -> bool {
        matches!(
            self,
            EnvironmentIssue::MissingLock
                | EnvironmentIssue::LockDrift
                | EnvironmentIssue::MissingArtifacts
                | EnvironmentIssue::MissingEnv
                | EnvironmentIssue::EnvOutdated
                | EnvironmentIssue::RuntimeMismatch
        )
    }
}
#[allow(dead_code)]
pub(crate) fn issue_from_details(details: &Value) -> Option<EnvironmentIssue> {
    EnvironmentIssue::from_details(details)
}
