## Workspace lock lacks workspace metadata
- **Repro:** create a workspace with members (e.g., two empty projects) and run `px sync` from the root; open `px.workspace.lock`.
- **Issue:** the lockfile only contains the generic project metadata; it does not record workspace members or package ownership, so consumers cannot tell which member owns which dependency or even that the lock is a workspace lock.
- **Fix:** emit workspace-specific metadata (members and owners) in `px.workspace.lock` and parse it back, keeping the manifest fingerprint tied to the workspace.

## Workspace add/remove leave manifests dirty on failed resolution
- **Repro:** inside a workspace member, run `px add bogus-package-px-does-not-exist==1.0.0`; the command fails, but `apps/a/pyproject.toml` now contains the bogus dependency and no lock/env were produced.
- **Issue:** workspace mutations update the member manifest before resolution and never roll back on failure, violating the spec requirement that failed adds/removes leave manifests/locks untouched.
- **Fix:** take a backup of the member manifest and workspace lock before mutation and restore it when resolution or sync fails.

## Empty workspace manifests crash status
- **Repro:** create a `pyproject.toml` with `[tool.px.workspace]` and `members = []`, then run `px status` in the root.
- **Issue:** `px status` aborts with an internal error ("workspace has no members defined") instead of reporting a valid workspace state. Spec allows an empty member list (initialized-empty workspace) and status must be user-facing.
- **Fix:** allow empty member lists, default the workspace python requirement sensibly when members are empty, and surface a normal workspace state report instead of panicking.
