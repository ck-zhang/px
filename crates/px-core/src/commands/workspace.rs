use anyhow::Result;

use crate::{workspace, CommandContext, ExecutionOutcome};

#[derive(Clone, Debug, Default)]
pub struct WorkspaceListRequest;

#[derive(Clone, Debug, Default)]
pub struct WorkspaceVerifyRequest;

#[derive(Clone, Debug)]
pub struct WorkspaceInstallRequest {
    pub frozen: bool,
}

#[derive(Clone, Debug, Default)]
pub struct WorkspaceTidyRequest;

pub fn workspace_list(
    ctx: &CommandContext,
    _request: WorkspaceListRequest,
) -> Result<ExecutionOutcome> {
    workspace::list(ctx)
}

pub fn workspace_verify(
    ctx: &CommandContext,
    _request: WorkspaceVerifyRequest,
) -> Result<ExecutionOutcome> {
    workspace::verify(ctx)
}

pub fn workspace_install(
    ctx: &CommandContext,
    request: WorkspaceInstallRequest,
) -> Result<ExecutionOutcome> {
    workspace::install(ctx, request.frozen)
}

pub fn workspace_tidy(
    ctx: &CommandContext,
    _request: WorkspaceTidyRequest,
) -> Result<ExecutionOutcome> {
    workspace::tidy(ctx)
}
