# Modularization Plan Hand‑Off (px)

This document captures the remaining refactors to finish the ongoing modularization work.

## Status
- Done: px-domain lockfile split (types/io/analysis/spec).
- Done: px-core lib wiring into config/context/outcome/progress.
- Done: px-core store split (cache/wheel/sdist/prefetch).
- Done: px-core distribution build split.
- Pending: px-core project, tools, migration splits (below).

## TODOs

### 1) Project module (`crates/px-core/src/project/`)
- Create submodules:
  - `mutate.rs`: add/remove/update flows, includes `ManifestLockBackup` and helper logic.
  - `sync.rs`: `project_sync` + `project_sync_outcome`, override resolution.
  - `state.rs`: `evaluate_project_state`, `ensure_mutation_allowed`.
  - `status.rs`: `project_status` and environment status helpers.
  - Keep existing `init.rs` and `why.rs`.
- `mod.rs` should be a thin facade:
  - Re-export public functions/requests (project_* APIs, Project*Request types).
  - Re-export crate-visible `MutationCommand`.
  - Pull shared helpers that need to be reused across submodules.
- Preserve public API signatures/behavior; adjust tests/imports accordingly.

### 2) Tools module (`crates/px-core/src/tools/`)
- Add submodules:
  - `paths.rs`: tool dir resolution, `normalize_tool_name`, `tool_root_dir`.
  - `metadata.rs`: `ToolMetadata` read/write, `InstalledTool`, `MIN_PYTHON_REQUIREMENT`.
  - `install.rs`: `tool_install`/`tool_upgrade` and dependency resolution.
  - `run.rs`: `tool_run`, env building, console handling.
  - `list_remove.rs`: `tool_list` and `tool_remove`.
- `mod.rs` re-exports public APIs and request structs; keep internal helpers `pub(crate)`.

### 3) Migration module (`crates/px-core/src/migration/`)
- Add submodules:
  - `plan.rs`: autopin/onboard planning (`prepare_pyproject_plan`, `apply_python_override`, `plan_autopin` helpers).
  - `apply.rs`: migrate command flows, `LockBehavior`, `AutopinPreference`, workspace policy handling.
  - `runtime.rs`: `fallback_runtime_by_channel` and runtime selection helpers.
- `mod.rs` re-exports public APIs/enums to preserve CLI-facing surface.

### 4) Distribution polish (optional)
- Already split build; if desired, move artifact summarizing/formatting into `distribution/artifacts.rs` and keep `publish.rs` consuming the facade only.

### 5) Cleanup/checks after each split
- Run `cargo fmt` and `cargo check -p px-core` (and `-p px-cli` if CLI paths change).
- Ensure visibility is correct: re-export from `mod.rs`, keep submodules `pub(crate)` unless needed.
- Move or adjust tests to follow their new modules; keep behavior/output identical.

## Notes
- Avoid changing public outputs or CLI messages; this is a structural refactor.
- Keep backup helpers (`ManifestLockBackup`) and state evaluation logic intact; just relocate.
- When in doubt, mirror the existing APIs and re-export so upstream callers remain untouched.
