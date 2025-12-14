#[derive(Clone, Copy, Debug)]
pub enum MigrationMode {
    Preview,
    Apply,
}

impl MigrationMode {
    pub(super) const fn is_apply(self) -> bool {
        matches!(self, Self::Apply)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum WorkspacePolicy {
    CleanOnly,
    AllowDirty,
}

impl WorkspacePolicy {
    pub(super) const fn allows_dirty(self) -> bool {
        matches!(self, Self::AllowDirty)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum LockBehavior {
    Full,
    LockOnly,
}

impl LockBehavior {
    pub(super) const fn is_lock_only(self) -> bool {
        matches!(self, Self::LockOnly)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum AutopinPreference {
    Enabled,
    Disabled,
}

impl AutopinPreference {
    pub(super) const fn autopin_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

#[derive(Clone, Debug)]
pub struct MigrateRequest {
    pub source: Option<String>,
    pub dev_source: Option<String>,
    pub mode: MigrationMode,
    pub workspace: WorkspacePolicy,
    pub lock_behavior: LockBehavior,
    pub autopin: AutopinPreference,
    pub python: Option<String>,
}
