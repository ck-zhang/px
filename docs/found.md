## px init leaves artifacts when runtime is missing
Description: `px init` aborted for lack of a registered Python runtime but still wrote project files.
Repro commands:
```bash
tmpdir=$(mktemp -d)
PX_RUNTIME_REGISTRY="$tmpdir/runtimes.json" (cd "$tmpdir" && px init)
ls -a "$tmpdir"
```
Expected: Command fails and the directory stays empty.
Actual: Command fails but `pyproject.toml` and `.px/` were created.
Root cause: `project_init` scaffolded files before installing deps and never rolled them back when installation failed.
Fix: Track scaffolded paths and restore/remove them when init fails before the runtime/env are ready.

## px update mutates manifest when lock write fails
Description: `px update` left `pyproject.toml` rewritten even though updating failed with a permission error.
Repro commands:
```bash
tmpdir=$(mktemp -d)
cd "$tmpdir" && px init
px add packaging==23.0
chmod 444 px.lock
px --json update packaging
rg "packaging" pyproject.toml px.lock
```
Expected: Update reports the failure but leaves both pyproject and px.lock unchanged.
Actual: pyproject was rewritten to `packaging==25.0` while px.lock stayed at 23.0 and the CLI printed a backtrace.
Root cause: `project_update` rewrote dependencies before install and lacked backups; install errors bubbled as internal errors and restore failed on read-only files.
Fix: Capture manifest/lock backups with permission-aware restore, wrap update errors as user-facing failures, and roll back on any failure.

## tool install scaffolds without a runtime
Description: Installing a tool with no px runtime configured failed but still created a tool directory and pyproject.
Repro commands:
```bash
tools=$(mktemp -d); store=$(mktemp -d); reg=$(mktemp)
PX_RUNTIME_REGISTRY="$reg" PX_TOOLS_DIR="$tools" PX_TOOL_STORE="$store" px tool install ruff
find "$tools" -maxdepth 2 -type f
```
Expected: Command fails cleanly and does not create `tools/ruff`.
Actual: `tools/ruff/pyproject.toml` was written even though the install aborted.
Root cause: `tool_install` resolved the runtime after creating the tool root, so runtime errors left partial scaffolding.
Fix: Resolve the required runtime before creating tool directories and add a guard test to ensure no files are written when runtime lookup fails.
