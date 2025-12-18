# Code map (mega-module splits)

This repo used to have a few "mega-modules" that were split into smaller files/directories for
navigability and reviewability. This note keeps a single place to find the new layout without
leaving large "mapping note" banners scattered throughout the code.

## `px-core`

### Runtime facade
`crates/px-core/src/core/runtime/facade/`
- `mod.rs`: public re-exports + wiring
- `context/`: command/runtime context assembly (PYTHONPATH, autosync, version-file)
- `plan.rs`: lock/manifest resolution + dependency planning helpers
- `env_materialize/`: materialized env/site layout + state.json persistence helpers
- `cas_native.rs`: CAS-native runner + consistency checks
- `sandbox.rs`: sandbox/sysroot compatibility helpers
- `execute.rs`: process/output -> `ExecutionOutcome` mapping helpers
- `errors.rs`: user-facing error/outcome shaping + JSON response helpers

### CAS environment helpers
`crates/px-core/src/core/runtime/cas_env/`
- `owners.rs`: env root + owner id helpers
- `fs_tree.rs`: filesystem tree copy + permission helpers
- `scripts.rs`: python shim + entrypoint script helpers
- `runtime.rs`: runtime header/archive helpers
- `materialize.rs`: CAS materialization (runtime/pkg-build/profile env)
- `profile.rs`: CAS profile assembly + dependency staging

### Run implementation
`crates/px-core/src/core/runtime/run/`
- `driver.rs`: high-level `px run` / `px test` entrypoints
- `cas_native.rs`: CAS-native execution path + fallbacks
- `ephemeral/`: `--ephemeral` flow (no working-tree writes)
- `sandbox/`: sandbox runner glue
- `test_exec/`: pytest/builtin/script test runners

### Execution planning
`crates/px-core/src/core/runtime/execution_plan/`
- `types.rs`: `ExecutionPlan` schema types
- `plan.rs`: workspace/project plan assembly
- `sandbox.rs`: sandbox plan assembly
- `sys_path.rs`: sys.path extraction from profile OIDs

### Sandbox integration
`crates/px-core/src/core/sandbox/`
- `pack/`: `px pack` implementation
- `image.rs`, `store.rs`, `resolve.rs`: image/store/definition assembly
- `system_deps.rs`: system dependency pinning + rootfs materialization
- `paths.rs`, `time.rs`: deterministic paths + timestamps

### CAS store
`crates/px-core/src/core/store/cas/`
- `archive.rs`: deterministic filesystem archiving helpers
- `doctor.rs`: integrity verification + self-healing
- `gc.rs`: garbage collection + env-driven policy glue
- `keys.rs`: deterministic lookup-key helpers
- `repo_snapshot/`: repo-snapshot objects + materialization
- `store_impl/`: core CAS store operations (object IO, refs, index, manifests)
- `store_impl/index/`: SQLite index split (ex-`store_impl/index.rs`)
  - `mod.rs`: layout ensure + index path
  - `connection.rs`: sqlite connection + tx helper
  - `schema.rs`: schema DDL
  - `meta.rs`: meta table + version enforcement
  - `health.rs`: integrity checks + rebuild trigger
  - `rebuild.rs`: reconstruct index by scanning store + state files
  - `permissions.rs`: harden file permissions
  - `objects.rs`: objects table helpers

### Sdist build/cache
`crates/px-core/src/core/store/sdist/`
- `ensure.rs`: build orchestration + cache match
- `download.rs`: downloads + cross-device persistence
- `builder.rs`: container builder glue
- `native_libs.rs`: native library scanning/copy
- `hash.rs`: build options hashing
- `wheel.rs`: wheel discovery

### Migration apply pipeline
`crates/px-core/src/core/migration/apply/`
- `types.rs`: request/config types
- `migrate.rs`: migrate entrypoint + flow
- `foreign_tools.rs`: foreign tool detection helpers
- `locked_versions.rs`: lock pin reuse helpers

## `px-domain`

### Manifest parsing/editing helpers
`crates/px-domain/src/project/manifest/`
- split by concerns: normalization, options, package collection, dependency groups, fingerprints
