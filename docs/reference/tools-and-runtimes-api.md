# Tools and Runtimes API

## Tools

### Concept

A **tool** is a named entry point (e.g. `black`, `pytest`) with its own locked env, CAS-backed and isolated from both project envs and other tools. Tools never modify project roots.

### Files and shape

Tools live under a global location (e.g. `~/.px/tools/`):

* One directory per tool name containing:

  * Tool metadata (runtime, main package, constraints).
  * `tool.lock` (similar to `px.lock` but tool-specific).
  * Tool env(s) tied to that lock.

### Commands

* `px tool install <name> [spec] [--python VERSION]`

  * Resolve and lock the specified package.
  * Bind to a chosen runtime (default: px’s default runtime; or explicit `--python`).
  * Materialize env for the tool.

* `px tool run <name> [args...]`

  * Look up tool by name.
  * Ensure the bound runtime is available and compatible.
  * Ensure env matches `tool.lock`.
  * Run the tool.

* `px tool list` – list installed tools, versions, and runtimes.
* `px tool remove <name>` – remove tool metadata, lock, and env(s).
* `px tool upgrade <name>` – re-resolve within constraints; update `tool.lock` and env.

### Tool runtime selection and Python upgrades

1. **Install-time binding**

   * Each tool is installed against a specific runtime version (from `--python` or px default).
   * That runtime version is recorded in `tool.lock`.

2. **Run-time resolution**

   * `px tool run` looks up the runtime recorded in `tool.lock`.
   * Uses a px-managed interpreter for that exact version if available.
   * If missing, fail clearly; do **not** fall back to another px-managed runtime or system Python.

3. **Upgrades**

   * If the runtime the tool was locked against is missing or incompatible, `px tool run` must fail with a PX error and suggest reinstalling for the current runtime or installing the missing runtime.
   * No silent breakage; no implicit re-resolution.

## Runtimes (`px python`)

### Concept

A **runtime** is a Python interpreter that px can discover, select for a project/tool, and record in config/locks.

### Project runtime resolution

Deterministic precedence:

1. `[tool.px.workspace].python` in the workspace root `pyproject.toml` (when inside a workspace).
2. `[tool.px].python` (explicit per-project setting, e.g. `"3.11"`).
3. `[project].requires-python` (PEP 621).
4. px default runtime.

If no available runtime satisfies constraints, commands must fail with a clear explanation and suggest `px python install`. px must not fall back to arbitrary system interpreters outside its runtime registry once a project is under px management.

### Commands

* `px python list` – show runtimes px knows, with version and path.
* `px python install <version>` – download/install CPython release (implementation detail) under `~/.px/runtimes/...` and register it.
* `px python use <version>` – record runtime choice and sync lock/env (writes `[tool.px].python` or `[tool.px.workspace].python`).
* `px python info` – show details about the active runtime for the current project and default tool runtime.
