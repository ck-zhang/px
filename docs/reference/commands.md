# Commands

Commands operate over the project or workspace state machines. Use this doc alongside [State machines](./state-machines.md) for allowed states and invariants.

For a quick inventory of CLI flags and environment toggles, see [Env Vars and Flags](./env-and-flags.md).

## Command surface

### Core project verbs

* `px init`    – Create a new px project and empty lock/env.
* `px add`     – Add dependencies and update lock/env.
* `px remove`  – Remove dependencies and update lock/env.
* `px sync`    – Resolve (if needed) and sync env from lock.
* `px update`  – Upgrade dependencies within constraints and sync env.
* `px run`     – Run a command inside the project env.
* `px test`    – Run tests inside the project env.
* `px fmt`     – Run formatters/linters/cleanup via px tools (project state read-only).
* `px status`  – Show project / lock / env / runtime status.

### Tools

* `px tool install` – Install a Python CLI as a px-managed tool.
* `px tool run`     – Run a tool in its isolated env.
* `px tool list`    – List installed tools.
* `px tool remove`  – Remove an installed tool.
* `px tool upgrade` – Upgrade tool env within constraints.

### Runtimes

* `px python list`    – List runtimes px knows about.
* `px python install` – Install a new runtime (e.g. 3.11).
* `px python use`     – Select runtime for the current project.
* `px python info`    – Show details about current runtime(s).

### Shell integration

* `px completions` – Print a shell completion setup snippet (one-time setup).

### Distribution / migration

* `px build`    – Build sdists/wheels into `dist/`.
* `px publish`  – Upload artifacts from `dist/` to a registry.
* `px pack image` – Build a sandbox-backed OCI image from the current env profile and `[tool.px.sandbox]`.
* `px pack app` – Build a portable `.pxapp` bundle runnable via `px run <file>.pxapp`.
* `px migrate`  – Plan migration of a legacy project into px.
* `px migrate --apply` – Apply that migration.

### Introspection

* `px explain` – Execution introspection (what px would execute, and why), without executing or repairing.
  * `px explain run [<same args as px run>]` – Show runtime/profile selection, engine path (`cas_native` vs `materialized_env`), argv/workdir/sys.path, and (when applicable) sandbox/source provenance.
  * `px explain entrypoint <name>` – Show which distribution provides a `console_scripts` entrypoint and its resolved `module:function` target.
* `px why` – Explain why a package or decision exists (`px why <package>` or `px why --issue <id>`).

`px explain` answers “what will run and how”, while `px why` answers “why is this dependency here”.

### Workspace-aware routing (overview)

Top-level commands stay the same. Their routing depends on whether a workspace root is found above CWD:

* No workspace root → commands operate on the project state machine.
* Workspace root above and CWD is inside a member project → commands operate on the workspace state machine for deps/env while still reading/writing that project’s manifest.
* At the workspace root, `px sync` / `px update` / `px status` operate on the workspace state machine by default.

There is no `px workspace` top-level verb; “workspace” is a higher-level unit that reuses the existing command surface.

## Command contracts

### `px init`

* **Intent**: initialize a new px project and create an empty, self-consistent environment.
* **Preconditions**: CWD is not inside an existing px project. If `pyproject.toml` exists but appears owned by another tool (e.g. Poetry-only), refuse and suggest `px migrate`.
* **Behavior**:

  * Create/update `pyproject.toml` with minimal `[project]` and `[tool.px]`.
  * Choose a runtime satisfying `requires-python` (prefer px-managed; otherwise process Python if compatible).
  * Create an empty `px.lock` for the chosen runtime.
  * Materialize a global env under `~/.px/envs/<profile_oid>` matching `px.lock` and update the local pointer at `.px/envs/current`.

* **Postconditions**: manifest, lock, and env exist; project is self-consistent.
* **Failure**: no partial lock/env; at worst, logs under `.px/logs/`.

### `px add <pkg>…`

* **Intent**: add dependencies and make them immediately available.
* **Behavior**: modify `[project].dependencies`, resolve with current runtime, write new `px.lock`, update project env.
* **Postconditions**: manifest and lock reflect new deps; env matches lock; project self-consistent.
* **Failure**: on resolution failure, no changes; error includes copy-pasteable fix.

### `px remove <pkg>…`

* **Intent**: remove direct dependencies and update the environment.
* **Behavior**: remove deps from `[project].dependencies`; re-resolve; write new `px.lock`; update env.
* **Constraints**: refuses to remove non-direct deps; suggests `px why <pkg>`.
* **Postconditions**: manifest/lock/env updated and self-consistent.

### `px sync [--frozen]`

* **Intent**: make the project environment match declared state.
* **Dev behavior**:

  * If lock missing or manifest drifted: resolve and write `px.lock`.
  * If env missing or stale: rebuild env from `px.lock`.

* **Frozen/CI**:

  * If lock missing or drifted: fail; never resolve.
  * If env missing/stale: rebuild env from existing lock.

* **Postconditions**: lock matches manifest; env matches lock; project self-consistent. Operations are transactional.

### `px update [<pkg>…]`

* **Intent**: upgrade dependencies to newer compatible versions and apply them.
* **Behavior**: update all deps or named ones within constraints; write updated `px.lock`; rebuild env.
* **Failure**: on resolution failure, no change; errors describe conflicting constraints and how to relax them.

### `px run <target> […args]`

* **Intent**: run a command using the project env with deterministic state behavior and deterministic target resolution.
* **Preconditions**:
  * **`.pxapp` bundle targets**: no local project is required; px executes the bundle in a sandbox and does not read or write `pyproject.toml`, lockfiles, or `.px/` from the current directory.
  * **Project targets**: project root exists. Lock must exist and match manifest (otherwise suggest `px sync`). In CI/`--frozen`, px never re-resolves; if a materialized env is required (e.g. `--sandbox` or compatibility fallback), it must already be consistent.
  * **Run-by-reference targets** (`gh:` / `git+`): no local project is required; px runs from a commit-pinned repository snapshot stored in the CAS and does not write `pyproject.toml`, `px.lock`, or `.px/` into the caller directory.
  * **Ephemeral / try mode** (`--ephemeral` / `--try`): no local px project is required; px derives a cached env profile from read-only inputs in the current directory and **never** writes `.px/` or `px.lock` into that directory.
    * Inputs (in order): PEP 723 `# /// script` metadata on the target script, otherwise `pyproject.toml`, otherwise `requirements.txt`, otherwise an empty env.
    * Workdir: the user’s current directory.
    * `CI=1` or `--frozen`: refuses unless all dependencies are fully pinned (must contain `==` / `===`), with a hint to adopt via `px migrate --apply`.
* **Target resolution**:

  0. **Execute a `.pxapp` bundle**:

     * If `<target>` is a filesystem path that exists and ends with `.pxapp`, px runs it as a portable sandbox app bundle.
     * All args after the bundle path are forwarded to the bundle’s entrypoint.
     * This mode does not support `--at` (commit-scoped execution).

  1. **Run by reference** (explicit prefixes only):

     * **GitHub shorthand**: `gh:ORG/REPO@<sha>:path/to/script.py`
     * **Git URL**: `git+file:///abs/path/to/repo@<sha>:path/to/script.py` (also supports `git+https://…@<sha>:…`)

     Semantics:
     * **Pinned by default**: `@<sha>` must be a full commit SHA; floating refs (branch/tag/no `@`) are rejected unless `--allow-floating`.
     * **Frozen/CI**: floating refs are refused even with `--allow-floating`.
     * **Offline** (`--offline` / `PX_ONLINE=0`): the repo snapshot must already be in the CAS; otherwise the run fails (no implicit network or git fetch).
     * **Locator hygiene**: `git+https://…` locators must not embed credentials, query strings, or fragments; use a git credential helper instead.
     * **Dependencies**: if the target script contains PEP 723 `# /// script` (or `# /// px`) metadata, px uses it; otherwise the script runs in an empty env.
     * **No project mutation**: the snapshot is materialized into px’s cache (read-only) and never touches the caller’s working directory.
     * **Current limitations**: run-by-reference currently supports Python scripts only and does not support `--sandbox` or `--at`.

  2. If `<target>` is a file under the project root, run it as a script with the project runtime.
  3. If `<target>` is a Python alias (`python`, `python3`, `py`, etc.), run the project runtime directly.
  4. If `<target>` matches a `console_scripts` entry point from the resolved environment, run that entry point.

     * CAS-native (default): px dispatches via stdlib `importlib.metadata` without relying on a prebuilt `bin/` tree.
     * If multiple distributions claim the same `console_scripts` name (or native dispatch fails for packaging quirks), px automatically falls back to materialized env execution and runs the deterministic `bin/` winner instead.
     * Materialized env fallback: uses the env’s `bin/` projection and PATH-based wrappers.

  5. Otherwise run `<target>` as an executable, relying on PATH from the project env.

  No implicit module/CLI guessing (`python -m`, `.cli`, etc.).

* **Argument parsing**: px flags must appear before `<target>`; once `<target>` is seen, all following tokens are forwarded verbatim (no `--` needed).

* **Behavior (dev)**: prefers CAS-native execution (no persistent env directory required). If a materialized env is needed for compatibility, px rebuilds it from `px.lock` (no resolution) and proceeds.
* **Behavior (CI/`--frozen`)**: fail if lock drifted or env stale; never repairs.
* **Sandbox mode (`--sandbox`)**: runs the target inside a sandbox image derived from `[tool.px.sandbox]` + the resolved env profile. Same state requirements as unsandboxed `px run` (dev may rebuild env; frozen requires `Consistent`). px may build/reuse the sandbox image but never mutates manifest/lock/env; working tree is bind-mounted into the container for execution so code edits are live.
* **Commit-scoped**: `px run --at <git-ref>` uses the `pyproject.toml` + lock from that ref (project or workspace) without checking it out; locks are treated as frozen (fail if missing/drifted), and envs are reused/materialized in the global cache without touching the working tree.
* **Env prep**: PATH is rebuilt with the px env’s `site/bin` first (px materializes console/gui scripts there from wheels); exports `PYAPP_COMMAND_NAME` when `[tool.px].manage-command` is set; runs a lightweight import check for `[tool.px].plugin-imports` and sets `PX_PLUGIN_PREFLIGHT` to `1`/`0`; clears proxy env vars.
* **Pip semantics**: px envs are immutable CAS materializations; mutating pip commands (`pip install`, `python -m pip uninstall`, etc.) are blocked with a PX error. px seeds pip + setuptools into the project site so legacy `setup.py`/`pkg_resources` flows keep working without declaring them explicitly. Read-only pip invocations (`pip list/show/help/--version`) run normally.
* **VCS version files**: if `[tool.hatch.build.hooks.vcs].version-file` points to a missing file, px writes one using `[tool.hatch.version.raw-options].git_describe_command` when set (otherwise `git describe --tags --dirty --long`), mirrors `version_scheme = "python-simplified-semver"` + `local_scheme = "no-local-version"` when those Hatch raw options are present, and falls back to `git rev-parse --short HEAD`; if git metadata is unavailable, px writes `0.0.0+unknown` as a safe fallback.
  Hatch VCS projects without a `version-file` still get a derived, parseable version in px’s editable metadata, respecting `local_scheme = "no-local-version"` (so local suffixes are dropped) and defaulting to `0.0.0` when px cannot read git metadata.
* **Failure hints**: missing module during execution → if dep absent in M/L suggest `px add <pkg>`; if present suggest `px sync`.
* **Stdin**: passthrough targets using `python -` keep stdin attached so piped scripts can run; other non-interactive runs keep stdin closed to avoid blocking.

### `px test`

* Same consistency semantics as `px run`. Prefers CAS-native execution and falls back to a materialized env when needed. Prefers project-provided runners like `tests/runtests.py` (or `runtests.py`) and otherwise runs `pytest` inside the project env.
* Supports `--sandbox` with the same sandbox definition/resolution rules as `px run`; working tree is bind-mounted for live code.
* `--at <git-ref>` mirrors `px run --at …`, using the manifest + lock at that ref with frozen semantics (no re-resolution; fail if lock is missing or drifted).
* `--ephemeral` / `--try` mirrors `px run --ephemeral`: derive a cached env from `pyproject.toml` / `requirements.txt` without adopting the directory (no `.px/` or `px.lock` writes). In `CI=1` or `--frozen`, dependencies must be fully pinned.
* Output style (default): px streams the runner stdout/stderr live and renders a compact report:

  ```
  px test  •  Python 3.x.y  •  pytest <version>
  root:   /path/to/project
  config: pyproject.toml

  collected N tests from M files in 0.00s

  tests/test_example.py
    ✓ test_happy_path                   0.00s
    ✗ test_failure_case                 0.00s


  FAILURES (1)
  -----------

  1) tests/test_example.py::test_failure_case

     AssertionError: expected X, got Y

     tests/test_example.py:10
       8   def test_failure_case():
       9       ...
     →10       assert something == expected

  RESULT   ✗ FAILED (exit code 1)
  TOTAL    N tests in 0.00s
  PASSED   ...
  FAILED   ...
  SKIPPED  ...
  ERRORS   ...
  ```

  The default reporter groups tests by file, aligns names/durations, numbers failures, and shows a short code excerpt. Set `PX_TEST_REPORTER=pytest` to use the native pytest reporter with px’s trimmed defaults (`--color=yes --tb=short -q`). px leaves warning handling to the project’s pytest configuration.

### `px fmt`

* **Intent**: run configured formatters/linters/cleanup tools via px-managed tool environments, without mutating project state.
* **Behavior**:

  * Uses px tool store (`~/.px/tools/...`), not the project env.
  * Does not resolve or update `px.lock`; does not rebuild project env in dev or CI.
  * May modify code via invoked tools.

* **Missing tools**: fail with a clear message and a suggestion like `px tool install ruff`.
* **Postconditions**: project manifest/lock/env unchanged; tool envs may be created/updated.

### `px pack image`

* **Intent**: freeze the current env profile plus `[tool.px.sandbox]` into a deterministic sandbox image for deploys/CI parity.
* **Preconditions**: project/workspace manifest present and env clean; fails if env is missing or stale (suggests `px sync`). Working tree must be clean by default; `--allow-dirty` overrides with a warning.
* **Behavior**: resolves sandbox base/capabilities → `sbx_id`; copies the working tree (respecting ignores) into the image; reuses or builds the sandbox image; supports `--tag`, `--out <path>` (OCI tar/dir), and `--push` to a registry. Uses the existing env contents; never re-resolves or mutates manifests/locks/envs.
* **Postconditions**: project/workspace state unchanged; sandbox image cached/addressable by `sbx_id`.

### `px pack app`

* **Intent**: package the current project/workspace into a portable `.pxapp` bundle runnable via `px run <bundle>.pxapp`.
* **Preconditions**: same as `px pack image` (manifest present; env clean; clean worktree by default unless `--allow-dirty` is passed).
* **Behavior**: derives `sbx_id` from the env profile + `[tool.px.sandbox]`, snapshots app code, and writes a single-file `.pxapp` bundle (default: `dist/<name>-<version>.pxapp`).
* **Postconditions**: project/workspace state unchanged; `.pxapp` written to disk; sandbox images may be built/reused as an implementation detail.

### `px status`

* **Intent**: read-only snapshot of manifest/lock/env alignment plus runtime identity.
* **Behavior**: default TTY output shows location, state bullets, runtime/env/lock lines; `--brief` emits a one-liner; `--json` returns a structured payload (context, project/workspace flags, runtime/env/lock, `next_action`).
* **Postconditions**: read-only (except logs); never touches manifests, locks, or environments.

### `px migrate` / `px migrate --apply`

* `px migrate` – reads legacy inputs (requirements.txt, Pipfile, poetry.lock, existing venv) and prints a proposed manifest/lock/env plan; no writes.
* `px migrate --apply` – applies the plan: updates `pyproject.toml`, writes `px.lock`, builds env under `.px/`; leaves legacy files untouched; must not leave partial state on failure. When px scaffolds or migrates a Hatch/Hatchling project and writes `pyproject.toml`, it ensures the `px-dev` group includes `tomli-w>=1.0.0` so px can emit TOML; expect that dev helper to be added if it was missing. If `pyproject.toml` declares dependency ownership under another tool (e.g. `[tool.poetry.dependencies]`), px refuses to apply and asks you to remove/convert those sections first.

### Workspace-aware semantics

When a workspace root is detected above CWD and CWD is inside a member project, commands operate over the workspace state machine for deps/env while still reading that project’s manifest:

* `px add/remove` (from a member) – modify member manifest, re-resolve workspace graph → update `px.workspace.lock`, rebuild workspace env. Per-project `px.lock` is not updated in this mode.
* `px sync` (member or workspace root) – if workspace lock missing or drifted: resolve union graph and write `px.workspace.lock`; ensure workspace env matches it; never touch per-project locks for members.
* `px update` (member or workspace root) – update workspace lock within constraints and rebuild workspace env.
* `px run` / `px test` (member) – prefers CAS-native execution from the workspace profile (no persistent env directory required). If a materialized env is needed for compatibility or `--sandbox`, px builds/reuses the workspace env from the workspace lock. In CI, lock drift is still a hard error; `--sandbox` continues to require a consistent workspace env.
* `px pack image` (workspace root or member) – requires workspace `Consistent`; builds/reuses sandbox image from workspace env + `[tool.px.sandbox]`; copies workspace member code into the image; no workspace writes.
* `px pack app` (workspace root or member) – requires workspace `Consistent`; writes a `.pxapp` bundle from workspace env + `[tool.px.sandbox]`; copies workspace member code into the bundle; no workspace writes.
* `px status` – at workspace root: report workspace state plus member manifest health; in a member: report workspace state and whether that member manifest is included; under the workspace root but outside members: emit a note about the non-member path.
