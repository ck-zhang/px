# ADR 2025-11-15 – px-core command handler decomposition

**Status**: Accepted  \
**Date**: 2025-11-15

## Context

`crates/px-core/src/lib.rs` currently inlines every CLI command handler, lock/manifest helper, installer, and workspace workflow. That monolith blurs crate boundaries, forces the CLI to pass JSON blobs (`PxCommand.args`) instead of typed data, and hides infrastructure details (resolver/network/cache) behind scattered `env::var` checks. Step 1 of the refactor plan is to inventory the existing entrypoints so we can assign each responsibility to the crate that should own it (px-lockfile, px-project, px-store, px-workspace, px-cache, px-runtime/px-python) and clarify which layer (CLI → application services → infrastructure) should execute it.

## Decision

1. Keep px-core focused on typed command requests, `CommandContext`, and orchestration, while domain-specific logic migrates into the purpose-built crates listed below.
2. Introduce a handler registry (trait-based dispatch) so px-cli forwards typed requests instead of `(group, name)` matches. Each handler delegates to an application service that calls infrastructure traits (artifact store, python runner, git, network) via dependency injection.
3. Extract lock, project, store, and workspace responsibilities into their crates with explicit public APIs. px-core re-exports only the types needed by the CLI.
4. Centralize configuration (env vars, flags, config files) inside `CommandContext` so commands stop reading `env::var` directly and tests can inject deterministic behavior.

## Command handler inventory

### Infra, workflow, store, and migrate commands

| Command | Current entrypoint (px-core) | Current responsibility summary | Destination owner | Layer after refactor |
| --- | --- | --- | --- | --- |
| `infra.env` | `handle_env` | Detect interpreter, print project/env metadata, surface pythonpath | `px-core::env` service calling `px-python` + `px-runtime` for context | CLI (clap) → EnvService (px-core app) → `px-python`/`px-runtime` infra |
| `infra.cache` | `handle_cache`, `cache_path_outcome`, `cache_stats_outcome`, `cache_prune_outcome` | Resolve cache root, compute stats, delete entries | `px-cache` crate exposes `CacheService` with path/stats/prune APIs; px-core holds thin adapter | CLI → CacheService (px-core) → `px-cache` infra |
| `workflow.run` | `handle_run`, `run_module_entry`, `run_passthrough` | Discover entry point, build env, call python runtime | `px-runtime` implements `PythonRunner`; px-core hosts `RunService` using typed request | CLI → RunService → `px-runtime` + `px-python` infra |
| `workflow.test` | `handle_test`, `run_builtin_tests` | Invoke managed test runner, interpret output | Same as above; `px-runtime` supplies process exec, px-core service configures tools | CLI → TestService → `px-runtime` infra |
| `output.build` | `handle_output_build`, `write_sdist`, `write_wheel` | Package project into sdists/wheels, summarize outputs | Split between `px-project` (metadata, layout), `px-store` (artifact writing) with px-core orchestrating | CLI → BuildService (px-core) → `px-project` + `px-store` infra |
| `output.publish` | `handle_output_publish` | Locate built artifacts, upload via HTTP | New `PublishService` in px-core calling `px-store::Publisher` (HTTP/client traits) | CLI → PublishService → `px-store` infra |
| `store.prefetch` | `handle_store_prefetch`, `handle_project_prefetch`, `handle_workspace_prefetch` | Prefetch wheels/sdists for project or workspace | `px-store` exposes `PrefetchPlanner`; `px-workspace` contributes member iteration; px-core selects mode | CLI → StorePrefetchService → `px-store` infra |
| `migrate.migrate` | `handle_migrate` (+ helpers: `collect_pyproject_packages`, `plan_autopin`, `BackupManager`, `install_snapshot`) | Analyze pyproject/requirements, pin deps, optionally rewrite files and lock | `px-project` owns autopin + scaffold, `px-lockfile` handles lock rendering/verification; px-core hosts `MigrateService` combining them | CLI → MigrateService → `px-project` + `px-lockfile` + `px-resolver` + `px-store` infra |

### Project, quality, lock commands

| Command | Entry point | Summary today | Destination owner | Layer |
| --- | --- | --- | --- | --- |
| `project.init` | `handle_project_init`, `scaffold_project`, `infer_package_name` | Bootstrap pyproject, .px layout, git safety | `px-project::Initializer` for layout + naming; px-core ensures typed request | CLI → ProjectInitService → `px-project` infra |
| `project.add` | `handle_project_add`, `read_dependencies`, `write_dependencies`, `upsert_dependency` | Mutate `[project.dependencies]` and write TOML | `px-project::ManifestEditor` with typed dependency APIs | CLI → ManifestService → `px-project` |
| `project.remove` | `handle_project_remove` + helpers | Remove dependencies by name, rewrite pyproject | Same as above | CLI → ManifestService |
| `project.install` | `handle_project_install`, `manifest_snapshot`, `install_snapshot`, `resolve_dependencies`, `prefetch_artifacts`, `refresh_project_site` | Snapshot manifest, resolve pins, download artifacts, update lock + .px site | `px-project::Installer` (manifest + context), `px-lockfile` (lock graph), `px-store` (fetch/cache), `px-runtime` (site regen) | CLI → InstallService → infra crates |
| `quality.tidy` | `handle_tidy`, `detect_lock_drift` | Validate lock consistency without writing | `px-lockfile::DriftChecker` | CLI → TidyService → `px-lockfile` |
| `lock.diff` | `handle_lock_diff`, `analyze_lock_diff`, `LockDiffReport` | Compare manifest vs lock, emit JSON summary | `px-lockfile::DiffService` | CLI → LockService |
| `lock.upgrade` | `handle_lock_upgrade`, `render_lockfile_v2` | Rewrite lock to schema v2 | `px-lockfile::Upgrader` | CLI → LockService |

### Workspace commands

| Command | Entry point | Summary today | Destination owner | Layer |
| --- | --- | --- | --- | --- |
| `workspace.list` | `handle_workspace_list`, `read_workspace_definition` | Parse `[tool.px.workspace]`, report members | `px-workspace::Definition` + query helpers | CLI → WorkspaceService → `px-workspace` |
| `workspace.verify` | `handle_workspace_verify`, `analyze_lock_diff` reuse | Validate each member’s manifest/lock status | `px-workspace::Verifier` backed by `px-project` + `px-lockfile` services | CLI → WorkspaceService |
| `workspace.install` | `handle_workspace_install`, `WorkspaceMemberReport`, `WorkspaceStats` | Iterate members, call install_snapshot, aggregate drift | `px-workspace::Installer` orchestrates `px-project::Installer` per member | CLI → WorkspaceService → `px-project`/`px-lockfile`/`px-store` |
| `workspace.tidy` | `handle_workspace_tidy` | Run tidy/drift per member | Same pattern as above | CLI → WorkspaceService |

## Major supporting entrypoints currently in px-core

| Area | Representative functions (crates/px-core/src/lib.rs) | Target crate/module |
| --- | --- | --- |
| Manifest discovery & editing | `manifest_snapshot`, `manifest_snapshot_at`, `current_project_root`, `read_dependencies`, `write_dependencies`, `infer_package_name`, `plan_autopin`, `prepare_pyproject_plan` | `px-project` (manifest + project context APIs) |
| Installer/resolution | `install_snapshot`, `resolve_dependencies`, `resolve_pins`, `resolve_specs`, `prefetch_specs_from_lock`, `refresh_project_site`, `materialize_project_site` | `px-project::Installer` (orchestrator) using `px-resolver` + `px-store` |
| Lock parsing/rendering | `maybe_load_lock_snapshot`, `load_lock_snapshot`, `parse_lock_snapshot`, `render_lockfile`, `render_lockfile_v2`, `analyze_lock_diff`, `LockSnapshot`, `LockDiffReport`, `verify_lock`, `collect_resolved_dependencies` | `px-lockfile` |
| Artifact/cache helpers | `resolve_cache_store_path`, `cache_path_outcome`, `cache_stats_outcome`, `prefetch_artifacts`, `cache_wheel`, `ensure_sdist_build`, `build_wheel_via_sdist` | `px-store` (artifact store) + `px-cache` (policy/stats) |
| Workspace data | `read_workspace_definition`, `WorkspaceMemberReport`, `WorkspaceStats`, `finalize_workspace_outcome` | `px-workspace` |
| CLI plumbing | `GlobalOptions`, `PxCommand`, `ExecutionOutcome`, `default_outcome`, `array_arg` | Stay in px-core but move into `command`/`cli` modules with typed equivalents |

These supporting entrypoints will be carved out first so the command handlers can depend on crate-specific APIs instead of reimplementing helpers locally.

## Layering and typed command context

The CLI refactor follows the agent-produced typed dispatch design:

1. **Typed requests** – Each command derives a `clap::Args` struct that converts into a domain request (e.g., `InstallRequest`, `WorkspaceListRequest`). Trait `CommandRequest` is a marker for compile-time validation.
2. **CommandContext** – Built once at process startup. It contains the merged `Config` (env vars like `PX_RESOLVER`, CLI flags, config files), resolved paths, cache roots, resolver/installer options, and `Arc` handles to infrastructure traits (`ArtifactStore`, `PythonRunner`, `GitClient`, `NetworkClient`).
3. **Handler registry** – Define `trait PxCommandHandler<R: CommandRequest> { fn handle(&self, ctx: &CommandContext, req: R) -> Result<ExecutionOutcome>; }`. Register handlers in a map/enum so px-cli no longer matches on `(group, name)`; it instantiates the typed request and hands it off.
4. **Dependency injection** – Infrastructure traits live in their crates (`px-store` implements `ArtifactStore`, `px-runtime` implements `PythonRunner`, etc.) and are injected via the context, enabling deterministic unit tests.

## Public API & cross-crate boundary summary

- **px-core (application layer)**: exposes `CommandContext`, typed request structs/enums, handler traits, and service facades (EnvService, ProjectInitService, InstallService, WorkspaceService, etc.). It coordinates config + routing but delegates filesystem/network mutations to other crates.
- **px-cli (CLI layer)**: keeps clap parsing/output formatting, but only interacts with px-core through typed requests and `ExecutionOutcome`. Style/table rendering moves into dedicated modules so CLI stays thin.
- **px-project**: becomes the authoritative home for manifest discovery/editing, installer orchestration, `.px` site management, migration/autopin, and workspace member manifest snapshots. Public API highlights: `ProjectSnapshot`, `ProjectInstaller::install`, `ManifestEditor`, `ProjectInitializer`, `AutopinPlanner`.
- **px-lockfile**: owns lock schema/types plus diff/upgrade/verification utilities. Public API: `LockSnapshot`, `LockGraph`, `LockDiffReport`, `LockRenderer`, `LockVerifier`.
- **px-store + px-cache**: wrap cache resolution, artifact downloads, sdist builds, and stats/prune operations through traits like `ArtifactStore`, `PrefetchPlanner`, `CacheService`.
- **px-workspace**: defines workspace manifest parsing, member iteration, and aggregation services (`WorkspaceDefinition`, `WorkspaceInstaller`, `WorkspaceReporter`).
- **px-runtime / px-python**: remain infrastructure crates providing `PythonRunner` + interpreter detection. They plug into the context as trait objects.

With this inventory and target API sketch, subsequent steps can move handlers out of `crates/px-core/src/lib.rs` incrementally—one command group at a time—while new typed requests and the `CommandContext` keep cross-crate interaction narrow and testable.

