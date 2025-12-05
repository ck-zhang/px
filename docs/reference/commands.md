# Commands

Commands operate over the project or workspace state machines. Use this doc alongside `docs/reference/state-machines.md` for allowed states and invariants.

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

### Distribution / migration

* `px build`    – Build sdists/wheels into `dist/`.
* `px publish`  – Upload artifacts from `dist/` to a registry.
* `px migrate`  – Plan migration of a legacy project into px.
* `px migrate --apply` – Apply that migration.

### Introspection

* `px why` – Explain why a package or decision exists (`px why <package>` or `px why --issue <id>`).

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
  * Create a project env under `.px/envs/...` matching `px.lock`.

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

### `px run <target> [-- …args]`

* **Intent**: run a command using the project env with deterministic state behavior and deterministic target resolution.
* **Preconditions**: project root exists. In dev, lock must exist and match manifest (otherwise suggest `px sync`). In CI/`--frozen`, env must already be in sync; no repairs.
* **Target resolution**:

  1. If `<target>` is a file under the project root, run it as a script with the project runtime.
  2. Otherwise run `<target>` as an executable, relying on PATH from the project env (PEP 621 console/gui scripts and `python` from the env take precedence).

  No implicit module/CLI guessing (`python -m`, `.cli`, etc.).

* **Behavior (dev)**: if env missing/stale, rebuild from `px.lock` (no resolution) before running.
* **Behavior (CI/`--frozen`)**: fail if lock drifted or env stale; never repairs.
* **Commit-scoped**: `px run --at <git-ref>` uses the `pyproject.toml` + lock from that ref (project or workspace) without checking it out; locks are treated as frozen (fail if missing/drifted), and envs are reused/materialized in the global cache without touching the working tree.
* **Env prep**: PATH is rebuilt with the px env’s `site/bin` first (px materializes console/gui scripts there from wheels); exports `PYAPP_COMMAND_NAME` when `[tool.px].manage-command` is set; runs a lightweight import check for `[tool.px].plugin-imports` and sets `PX_PLUGIN_PREFLIGHT` to `1`/`0`; clears proxy env vars.
* **Pip semantics**: px envs are immutable CAS materializations; mutating pip commands (`pip install`, `python -m pip uninstall`, etc.) are blocked with a PX error. Read-only pip invocations (`pip list/show/help/--version`) run normally.
* **VCS version files**: if `[tool.hatch.build.hooks.vcs].version-file` points to a missing file, px writes one using `[tool.hatch.version.raw-options].git_describe_command` when set (otherwise `git describe --tags --dirty --long`), mirrors `version_scheme = "python-simplified-semver"` + `local_scheme = "no-local-version"` when those Hatch raw options are present, and falls back to `git rev-parse --short HEAD`; if git metadata is unavailable, px writes `0.0.0+unknown` as a safe fallback.
* **Failure hints**: missing module during execution → if dep absent in M/L suggest `px add <pkg>`; if present suggest `px sync`.
* **Stdin**: passthrough targets using `python -` keep stdin attached so piped scripts can run; other non-interactive runs keep stdin closed to avoid blocking.

### `px test`

* Same consistency semantics as `px run`. Prefers project-provided runners like `tests/runtests.py` (or `runtests.py`) and otherwise runs `pytest` inside the project env.
* `--at <git-ref>` mirrors `px run --at …`, using the manifest + lock at that ref with frozen semantics (no re-resolution; fail if lock is missing or drifted).
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
* `px run` / `px test` (member) – in dev may rebuild workspace env from lock; in CI requires workspace `Consistent`; always use workspace env.
* `px status` – at workspace root: report workspace state plus member manifest health; in a member: report workspace state and whether that member manifest is included; under the workspace root but outside members: emit a note about the non-member path.
