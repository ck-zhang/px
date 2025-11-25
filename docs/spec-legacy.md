> Legacy: this is the original monolithic spec. Current, modular docs live under `docs/index.md`.

# px Spec (Authoritative, state-machine focused)

---

## 0. What px is

px is the **front door for Python**:

* It owns **projects**, **tools**, and **runtimes**.
* It decides **which Python** runs, **which packages** are on `sys.path`, and **where they come from**.

Once px is in charge, you should never have to ask:

* “Which Python is this?”
* “Why is this dependency even here?”
* “Why did upgrading Python break my CLI tools?”

px is **not** a general task runner, multi-language build tool, or plugin marketplace.

---

## 1. Mental model (updated)

### 1.1 Nouns

px exposes **four** primary concepts:

* **Project**
  A directory with `pyproject.toml` and/or `px.lock` that px manages end-to-end.

* **Workspace**
  A set of related px projects in one tree that share a **single dependency universe** (one lock, one env) for development.

* **Tool**
  A named Python CLI installed into its own isolated, CAS-backed environment, runnable from anywhere.

* **Runtime**
  A Python interpreter (e.g. 3.10, 3.11) that px knows about and can assign to projects, tools, and workspaces.

Everything else – envs, lockfiles, caches – are implementation details.

### 1.2 Project lifecycle (intended story)

For a typical user, Python “with px” looks like:

1. `px init` – declare this directory as a px project.
2. `px add ...` – declare dependencies.
3. `px sync` – resolve, lock, and build the environment.
4. `px run ...` / `px test` / `px fmt` – execute in deterministic envs.
5. Commit `pyproject.toml` + `px.lock`.

That’s the core loop.

### 1.3 Tools lifecycle

For global tools:

1. `px tool install black`
2. `px tool run black --check .`

Tools are isolated from projects and from each other. Upgrading Python or changing project deps must not silently break them.

### 1.4 Design principles (updated)

px’s behavior is governed by three principles:

1. **Two parallel state machines, same shape**

   * Each **project** is described by three artifacts (M, L, E) and a small state machine (§10).
   * Each **workspace** is described by three artifacts (WM, WL, WE) and an analogous state machine (§11).
   * All commands are defined as transitions over one of these machines (or are read-only).
     If a workspace governs a project, the **workspace** machine is authoritative for deps/env; the project’s own lock/env are only used when the project is standalone.

2. **Determinism**

   Given the same project/workspace, runtimes, and configuration, px must make the same decisions: same runtime, same lockfile(s), same env(s), same command resolution (§3.5, §10.8, §11.6).

3. **Smooth UX, explicit mutation**

   * Mutating operations are explicit (`init`, `add`, `remove`, `sync`, `update`, `migrate --apply`, workspace sync/update, `tool install/upgrade/remove`).
   * “Reader” commands (`run`, `test`, `fmt`, `status`, `why`) never change manifests or lockfiles, and have tightly bounded behavior when they repair envs (if they ever do).

### 1.5 Workspace lifecycle (new)

For a multi-project repo, Python “with px” looks like:

1. `px init` in member projects – declare each directory as a px project.
2. Configure a workspace at the repo root (see §2.1, §11.1) listing member projects.
3. `px sync` (from the workspace root or any member) – resolve **across all members**, write a workspace lock, and build a shared env.
4. `px run` / `px test` inside any member – execute in that shared workspace env.
5. Commit:

   * the workspace manifest + workspace lock, and
   * each member’s `pyproject.toml` (and optionally its own `px.lock` if you use it standalone).

---

## 2. Filesystem & project/workspace shape (updated)

### 2.1 Roots & discovery

px distinguishes **project roots** and **workspace roots**.

A directory is a **px project root** if it contains:

* `pyproject.toml` with `[tool.px]`, or
* `px.lock`.

A directory is a **px workspace root** if it contains:

* `pyproject.toml` with `[tool.px.workspace]`.

Workspace and project roots may coincide (the workspace root can also be a project root), but they are conceptually separate.

**Project-level command discovery:**

1. Starting from CWD, walk upward until you find a **workspace root**.
2. If found and CWD is inside a listed member project, that project is **workspace‑governed**: project commands use the workspace state machine (§11.6).
3. Otherwise, walk upward to find a **project root** (no workspace above).
4. If none is found:
   `No px project found. Run "px init" in your project directory first.`

**Workspace-level discovery:**

* To reason about workspaces (status, sync, etc.), px finds the nearest workspace root above CWD (if any) via `[tool.px.workspace]`.

### 2.2 px-owned artifacts in a project/workspace (updated)

px may create/modify only:

* **User-facing / shared (per project):**

  * `pyproject.toml`

    * px edits only `[project]` (PEP 621) and `[tool.px]` sections.

  * `dist/`

    * build artifacts (sdist, wheels).

* **px-specific (per project):**

  * `px.lock` – locked dependency graph for this **project** when it is managed standalone (no governing workspace).
  * `.px/` – all internal state:

    * `.px/envs/` – envs owned by this project or workspace (see below).
    * `.px/logs/` – logs.
    * `.px/state.json` – metadata (current env ID(s), stored lock_id(s), runtime/platform fingerprints; validated and rewritten atomically).

* **px-specific (per workspace root):**

  * `[tool.px.workspace]` in `pyproject.toml` – workspace manifest (WM), including:

    * list of member project paths (relative to workspace root),
    * optional shared settings (e.g. default runtime, index config).
  * `px.workspace.lock` – workspace lock (WL) describing the **union** dependency graph of all members.
  * Workspace env metadata under `.px/` at the workspace root (WE). Physical layout is an implementation detail, but:

    * Workspace envs are distinguishable from per-project envs via px metadata.
    * A workspace env is always tied to `px.workspace.lock` and a runtime.

px **must not** create other top-level files or directories.

### 2.3 Shape after key commands

* After `px init` in empty dir:

  ```text
  myapp/
    pyproject.toml
    px.lock
    .px/
  ```

* After `px add` / `px sync`:

  Same as above, with `pyproject.toml` dependencies and `px.lock` graph populated, and `.px/envs/...` containing a built env.

* After `px build`:

  ```text
  myapp/
    pyproject.toml
    px.lock
    .px/
    dist/
      myapp-0.1.0.tar.gz
      myapp-0.1.0-py3-none-any.whl
  ```

For a workspace, the root has the workspace manifest/lock/env metadata plus the usual project artifacts if it is also a project.

---

## 3. Global concepts (updated)

### 3.1 Lockfiles (updated)

There are now two kinds of lockfiles:

* **Project lockfile** – `px.lock` in a project root:

  * Authoritative description of that project’s environment **when the project is not governed by a workspace**.
  * Exact versions, hashes, markers, index URLs, platform tags.
  * A fingerprint of `[project].dependencies` (and any px-specific dep config).

* **Workspace lockfile** – `px.workspace.lock` in a workspace root:

  * Authoritative description of the **shared** environment for all member projects.
  * Full resolved dependency graph across all members.
  * Mapping of each package node to its owning project (member) or “external”.
  * A **workspace manifest fingerprint** of the combined member manifests (§11.2.1).

Both lockfiles are **machine-generated only**; direct edits are unsupported.

### 3.2 Project environment (unchanged in meaning)

A **project environment** is a px-managed environment under `.px/envs/...` tied to:

* a project lock (`px.lock`) and
* a runtime/platform.

It must contain exactly the packages described by that project’s lock.

Project envs are used only when the project is **not** governed by a workspace. In a workspace, member projects use the workspace env (§3.3, §11.6).

### 3.3 Workspace environment (new)

A **workspace environment** is a px-managed environment under `.px/envs/...` at the workspace root:

* Tied to:

  * `px.workspace.lock` (WL) hash,
  * runtime (e.g. Python 3.11),
  * platform.

* Contains exactly the packages described by WL.

When a project is a **workspace member**, `px run` / `px test` / `px sync` for that project use the workspace env; per-project envs are not used in that context.

### 3.4 Self-consistent project/workspace (updated)

A project is **self-consistent** if:

* `pyproject.toml` and **its governing lock** agree:

  * standalone project → `px.lock` fingerprint matches;
  * workspace‑governed project → its manifest is included in the workspace manifest fingerprint, and WL matches that combined fingerprint.
* There exists an environment whose identity matches that lock (project env or workspace env).
* `px status` reports: `Environment in sync with lock`.

A workspace is **self-consistent** if:

* `[tool.px.workspace]` exists and matches `px.workspace.lock` (workspace manifest fingerprint).
* A workspace env exists and matches `px.workspace.lock`.
* `px status` at the workspace root reports the workspace as `Consistent` (§11.4).

All mutating commands must either:

* Leave the relevant object (project or workspace) self-consistent on success, or
* Fail without partial changes.

### 3.5 Deterministic surfaces (extended)

For a fixed px version, runtime set, platform, and index configuration, the following surfaces must be deterministic:

1. **Runtime selection**

   * Project runtime resolution follows a fixed precedence (§7.2).
   * Tool runtime resolution follows a fixed precedence (§6.4).
   * px must never “guess” a different runtime across runs for the same inputs.

2. **Lockfile generation**

   * Given manifest M, runtime, platform, and index configuration, resolver must produce the same `px.lock` (including ordering, `mfingerprint`, and lock ID).

3. **Environment materialization**

   * Given a `px.lock` and runtime, the environment E must contain exactly the packages described by L.
   * Rebuilding E for the same L must result in an equivalent environment (from px’s metadata point of view).

4. **Target resolution for `px run`**

   * For a given invocation, px must resolve the target using a fixed, documented rule (§5.6). No hidden fallbacks like `<package>.cli` are permitted.

5. **Non-TTY output and `--json`**

   * Under non-TTY stderr or `--json`, px must not emit interactive spinners or frame-based progress.
   * Output must be line-oriented or structured JSON, with stable shapes and ordering (§8.4).

6. **Error codes and shapes**

   * A given failure mode must map to a stable PX error code and “Why/Fix” structure (§8.1). The wording may improve, but semantics remain.

7. **Workspace lockfile generation**

   * Given workspace manifest WM (member list + their manifests), runtime, platform, and index configuration, the resolver must produce the same `px.workspace.lock` (including ordering, `wmfingerprint`, and workspace lock ID).

8. **Workspace environment materialization**

   * Given `px.workspace.lock` and runtime, the workspace environment WE must contain exactly the packages described by WL.
   * Rebuilding WE for the same WL must result in an equivalent environment (from px’s metadata point of view).

The project state machine in §10 and the workspace state machine in §11 are the reference models tying all of this together.

### 3.6 Dependency groups (canonical selection)

* Active dependency groups are controlled by `[tool.px.dependencies].include-groups` (PEP 503–normalized names). This list is authoritative for resolution, locking, and env sync.
* If `include-groups` is absent, px enables all declared groups: entries under `[dependency-groups]` and common dev-style optional deps (`dev`, `test`, `doc`, `px-dev`, etc.). `PX_GROUPS` can extend this set at runtime.
* The selected groups are part of the manifest fingerprint and lock drift detection for both projects and workspaces, keeping state transitions deterministic.
* `px migrate --apply` writes `include-groups` covering all declared groups so migrated projects get dev/test/doc dependencies without extra setup.

---

## 4. Command surface

### 4.1 Core project verbs

* `px init`    – Create a new px project and empty lock/env.
* `px add`     – Add dependencies and update lock/env.
* `px remove`  – Remove dependencies and update lock/env.
* `px sync`    – Resolve (if needed) and sync env from lock.
* `px update`  – Upgrade dependencies within constraints and sync env.
* `px run`     – Run a command inside the project env.
* `px test`    – Run tests inside the project env.
* `px fmt`     – Run formatters/linters/cleanup via px tools (project state read-only).
* `px status`  – Show project / lock / env / runtime status.

### 4.2 Tools

* `px tool install` – Install a Python CLI as a px-managed tool.
* `px tool run`     – Run a tool in its isolated env.
* `px tool list`    – List installed tools.
* `px tool remove`  – Remove an installed tool.
* `px tool upgrade` – Upgrade tool env within constraints.

### 4.3 Runtimes

* `px python list`    – List runtimes px knows about.
* `px python install` – Install a new runtime (e.g. 3.11).
* `px python use`     – Select runtime for the current project.
* `px python info`    – Show details about current runtime(s).

### 4.4 Distribution / migration

* `px build`    – Build sdists/wheels into `dist/`.
* `px publish`  – Upload artifacts from `dist/` to a registry.
* `px migrate`  – Plan migration of a legacy project into px.
* `px migrate --apply` – Apply that migration.

### 4.5 Introspection

* `px why` – Explain why a package or decision exists.

  * `px why <package>`
  * `px why --issue <id>`

### 4.6 Workspace-aware routing (overview)

You can keep the same top-level commands. Their **routing** now depends on whether a workspace root is found above CWD (§2.1, §5.11, §11.6):

* If no workspace root applies → commands operate on the **project state machine** (§10).
* If a workspace root applies and CWD is inside a member project → commands operate on the **workspace state machine** for deps/env, while still reading/writing that project’s manifest.
* At the workspace root:

  * `px sync` / `px update` / `px status` operate on the workspace state machine by default.

You still do **not** expose `px workspace` as a separate top-level verb; “workspace” is a higher-level unit that reuses the existing command surface. There is also no `px cache`, `px env`, or `px lock` top-level command.

---

## 5. Command contracts (authoritative semantics)

Command preconditions and end-states are defined against the canonical project states in [§10 Project state machine](#10-project-state-machine) and, when routed via a workspace, the workspace states in §11. Per-command invariants are summarized in the table in §10.8.

### 5.1 `px init`

**Intent**
Initialize a new px project and create an empty, self-consistent environment.

**Preconditions**

* CWD is not inside an existing px project (no project root above).
* If `pyproject.toml` exists:

  * Either minimal/empty, or
  * Not clearly owned by a conflicting tool (e.g. Poetry-only).
    In that case, `px init` must refuse and suggest `px migrate`.

**Behavior**

* Create or update `pyproject.toml` with:

  ```toml
  [project]
  name = "<directory-name>"
  version = "0.1.0"
  requires-python = ">=3.11"
  dependencies = []

  [tool.px]
  # px-specific settings
  ```

* Choose a runtime satisfying `requires-python`:

  * Prefer a px-managed runtime if present.
  * Otherwise use the px process Python; if incompatible, error with suggestion to install a suitable runtime.

* Create an empty `px.lock` for the chosen runtime (no deps).

* Create a project env under `.px/envs/...` matching `px.lock`.

**Postconditions (success)**

* `pyproject.toml`, `px.lock`, `.px/` exist.
* Project is self-consistent (`InitializedEmpty` / `Consistent`).

**Failure behavior**

* If non-px tool owns the project: refuse and suggest `px migrate`.
* On any error, do not leave partial `px.lock` or env; at worst, a `.px/logs/` entry.

---

### 5.2 `px add <pkg>…`

**Intent**
Add one or more dependencies and make them immediately available.

**Preconditions**

* Project root exists.
* `pyproject.toml` with `[project]` present.

**Behavior**

* Modify `[project].dependencies` to include the new requirements.
* Run the resolver with updated deps and current runtime.
* Write a new `px.lock` with resolved graph.
* Update project env under `.px/envs/...` to match `px.lock`.

**Postconditions (success)**

* `pyproject.toml` and `px.lock` reflect the new deps.
* Project env matches `px.lock`.
* Project is self-consistent.

**Failure behavior**

* On resolution failure:

  * Do not change `pyproject.toml`, `px.lock`, or env.
  * Report error in **What / Why / Fix** format with a copy-pasteable suggestion.

---

### 5.3 `px remove <pkg>…`

**Intent**
Remove one or more direct dependencies and update the environment.

**Preconditions**

* Same as `px add`.
* Each named package is either:

  * A direct dependency in `[project].dependencies`, or
  * px reports that it is only transitive and refuses.

**Behavior**

* Remove direct dependencies from `pyproject.toml`.
* Re-resolve, write a new `px.lock`.
* Update env to match `px.lock`.

**Postconditions (success)**

* `pyproject.toml` and `px.lock` reflect removal.
* Env matches `px.lock`.
* Project is self-consistent.

**Failure behavior**

* If a package is not a direct dep:

  * Refuse with:
    `"<pkg> is not a direct dependency; px why <pkg> for more."`

* On resolution failure:

  * No change to `pyproject.toml`, `px.lock`, or env.

---

### 5.4 `px sync [--frozen]`

**Intent**
Make the project environment match declared state.

**Preconditions**

* Project root exists.

**Behavior – dev (default)**

1. **Lockfile phase**

   * Compute fingerprint of `[project].dependencies` (+ px dep config).
   * If `px.lock` is missing or fingerprint mismatch:

     * Run resolver, write fresh `px.lock`.

2. **Environment phase**

   * If env is missing or its identity (hash/runtime) differs from `px.lock`:

     * Rebuild env from `px.lock`.

**Behavior – `--frozen` / `CI=1`**

* If `px.lock` is missing or manifest fingerprint mismatch:

  * Fail: `px.lock missing or out of date; update locally and commit.`
  * Do **not** run resolver.
* Else (manifest + lock agree):

  * If env is missing or stale, rebuild env from existing `px.lock`.

**Postconditions (success)**

* `px.lock` exists and matches `pyproject.toml`.
* Env matches `px.lock`.
* Project is self-consistent.

**Failure behavior**

* Env must not be left half-updated; operations should be transactional.
* Under `--frozen`/`CI=1`, no resolution is performed.

---

### 5.5 `px update [<pkg>…]`

**Intent**
Upgrade dependencies to newer compatible versions and apply them.

**Preconditions**

* Project root exists.
* `px.lock` exists (otherwise user must run `px sync` first).

**Behavior**

* Without args:

  * Attempt to update all dependencies to the newest versions allowed by constraints.

* With specific packages:

  * Attempt to update only the named packages (and their transitive graph) within constraints.

* Write updated `px.lock`.

* Update env to match `px.lock`.

**Postconditions (success)**

* `px.lock` reflects newer versions (within constraints).
* Env matches `px.lock`.
* Project is self-consistent.

**Failure behavior**

* On resolution failure:

  * No change to `px.lock` or env.
  * Error must describe which constraints conflict and how to relax them.

---

### 5.6 `px run <target> [-- …args]`

**Intent**
Run a command using the project env, with deterministic state behavior and deterministic target resolution.

**Preconditions**

* Project root exists.

* In dev mode:

  * `px.lock` must exist and match `pyproject.toml`.
    If not, error and suggest `px sync`.

* In CI / `--frozen` mode:

  * Env must already be in sync; `px run` does **not** repair anything.

**Target resolution (authoritative)**

Given `px run <target> -- <args>…`:

1. **Script alias (if configured)**

   * If `[tool.px.scripts].<target>` exists, expand it to a concrete command line and run that inside the project env.

2. **Direct command / script**

   * Otherwise, treat `<target>` as an executable or script path to run with the project env’s PATH and Python:

     * If `<target>` resolves to a file under the project root, run it as a script with the project runtime.
     * Else, run `<target>` as a process, relying on PATH from the project env (so console scripts and `python` from the env take precedence).

3. **No implicit module/CLI guessing**

   * px **must not** implicitly transform `<target>` into `python -m <target>` or `<target>.cli` or similar.
   * If the user wants module execution, they must say so (`px run python -m myapp.cli`) or define a `[tool.px.scripts]` alias.

This makes `px run` behavior fully deterministic and avoids surprising `ModuleNotFoundError` from “helpful” fallbacks.

**Behavior – dev**

* If env is missing or does not match `px.lock`:

  * Rebuild env from `px.lock` before running (E repair is allowed).
* Execute the resolved target inside the project env.

**Behavior – `--frozen` / `CI=1`**

* If `px.lock` missing or out-of-sync with `pyproject.toml`: fail and suggest `px sync`.
* If env not in sync with `px.lock`: fail.
* Never repairs envs in this mode.

**Postconditions (success)**

* If env was rebuilt, project is self-consistent.
* Otherwise, project consistency unchanged.

**Failure behavior**

* If no px project found:

  `No px project found. Run "px init" in your project directory first.`

* If there is no `px.lock` or it’s out-of-date:

  Suggest `px sync`.

* If a `ModuleNotFoundError` points to a missing dep during target execution:

  * Inspect project state (M, L):

    * If dep is not in M/L: suggest `px add <pkg>`.
    * If it is in M/L: suggest `px sync`.

---

### 5.7 `px test`

**Intent**
Run tests in the project env, mirroring `px run`’s consistency rules.

**Preconditions & behavior**

* Same consistency semantics as `px run` (with respect to project M/L/E):

  * In dev: may rebuild env from `px.lock` (no resolution).
  * In CI/`--frozen`: fails if lock/env out of sync; no repair.

* Discovers and runs the configured test runner (e.g. `pytest` by default) inside the project env.

**Postconditions**

* Same as `px run`.

---

### 5.8 `px fmt`

**Intent**
Run configured formatters/linters/cleanup **tools** via their px-managed tool environments, without mutating project state (M/L/E).

**Preconditions**

* Project root exists (so px can find `pyproject.toml` and `[tool.px]` config).
* Required tools are either:

  * installed via `px tool install`, or
  * px reports which tools are missing.

**Behavior**

* `px fmt` uses px’s **tool store** (`~/.px/tools/...`), not the project’s env:

  * Tools (e.g. `black`, `ruff`) run in their own locked environments (§6).
  * `px fmt` must not create, update, or rebuild the project env or `px.lock`.

* Project state:

  * `px fmt` does **not** resolve or update `px.lock`.
  * `px fmt` does **not** rebuild the project env, in dev or CI.
  * `px fmt` may read `[tool.px]` and other config in `pyproject.toml`, but must not modify `pyproject.toml`.

* Codebase:

  * Code may be modified by the invoked tools (formatting, lint fixes, etc.).
  * `px fmt` supports `--json` structured output like other commands.

* Missing tools:

  * If a required tool isn’t installed, `px fmt` must fail with:

    * a clear error message, and
    * a suggestion like: `px tool install ruff`.

**Postconditions**

* `pyproject.toml`, `px.lock`, and the project env (E) are unchanged.
* Tool envs may be created/updated as part of `px tool` lifecycle, but this is separate from the project state machine.

---

### 5.9 `px status`

**Intent**
Report project health without changing anything.

**Preconditions**

* Project root exists.

**Behavior**

* Summarize:

  * Project root path, project name.

  * Active runtime (version, path).

  * `px.lock`:

    * present / missing / out-of-sync with `pyproject.toml`.

  * Environment:

    * present / missing / in-sync / out-of-sync with `px.lock`.

* May also print the derived project state (`Uninitialized`, `NeedsLock`, `NeedsEnv`, `Consistent`) for diagnostics.

**Postconditions**

* None; read-only (except logs under `.px/logs/`).

---

### 5.10 `px migrate` / `px migrate --apply`

**Intent**
Convert a legacy Python project into a deterministic px project.

#### `px migrate` (preview)

* Reads legacy inputs:

  * e.g. `requirements.txt`, `Pipfile`, `poetry.lock`, existing venv.

* Computes a proposed:

  * `pyproject.toml`,
  * `px.lock`,
  * env plan.

* Prints a human-readable summary, **no writes** to M/L/E.

#### `px migrate --apply`

* Applies the plan:

  * Creates/updates `pyproject.toml` with `[project]` and `[tool.px]`.
  * Writes `px.lock`.
  * Builds env under `.px/`.

* Leaves legacy files (e.g. `requirements.txt`) untouched, optionally recording migration in `[tool.px.migration]`.

**Failure behavior**

* On ambiguous sources (multiple conflicting dep sources):

  * Refuse and require explicit `--from` choice.

* On failure, do not leave partial `pyproject.toml`/`px.lock`/env.

---

### 5.11 Workspace-aware routing

For any command that operates on M/L/E (`init`, `add`, `remove`, `sync`, `update`, `run`, `test`, `status`):

1. **Detect workspace context**

   * Find nearest workspace root above CWD.
   * If found, and CWD is inside a project listed as a member in `[tool.px.workspace].members`:

     * **Workspace-governed project**:

       * Reads/writes **project manifest** (that member’s `pyproject.toml`).
       * Reads/writes **workspace lock/env** (WL/WE), not per-project lock/env.
   * If no applicable workspace root:

     * Use **project state machine** (M/L/E, `px.lock`, project env).

2. **Semantics for key commands in workspace context**

   * `px add/remove` (from a member):

     * Modify that member’s `[project].dependencies`.
     * Re-resolve **workspace** graph → update `px.workspace.lock`.
     * Rebuild workspace env WE.
     * End state: workspace `Consistent` (§11.4); member’s deps are reflected via WL.
     * Per-project `px.lock` is not written/updated in this mode.

   * `px sync` (from a member or the workspace root):

     * Operates on WM/WL/WE:

       * If `px.workspace.lock` missing or out-of-date w.r.t WM: resolve union graph, write WL.
       * Ensure WE matches WL.
     * Does **not** touch per-project `px.lock` for members.

   * `px update` (from a member or the workspace root):

     * Operates on WL (updating versions within constraints across all members).
     * Rebuild WE.
     * Member projects see updated deps via WL.

   * `px run` / `px test` (from a member):

     * Require workspace `Consistent` or `NeedsEnv` (like project `run`/`test`).
     * In dev: may rebuild WE from WL (no workspace re-resolution).
     * In CI/`--frozen`: must see workspace `Consistent`; never repairs WE.
     * Always use WE; never per-project envs for members.

   * `px status`:

     * At workspace root: report workspace state (WM/WL/WE) plus a summary of each member’s manifest health.
     * In a member: report both:

       * workspace state, and
       * that project’s manifest status (e.g. “member manifest included in workspace fingerprint: yes/no”).

3. **Standalone vs workspace-governed projects**

   * A project that is **not** listed as a workspace member uses the **project** state machine exclusively (§10).
   * A project that **is** listed as a member and has a workspace root above it uses:

     * project manifest M in its own directory,
     * workspace lock/env (WL/WE) for everything that would normally use L/E.

This guarantees there is only one authority for deps/env at a time:

* either the **project** state machine (standalone), or
* the **workspace** state machine (for members).

---

## 6. Tools

### 6.1 Concept

A **tool** is:

* A named entry point (e.g. `black`, `pytest`)…
* With its own **locked** env, CAS-backed, isolated from both:

  * project envs, and
  * other tools.

Tools never modify project roots.

### 6.2 Files & shape

px stores tools under a global location (e.g. `~/.px/tools/`):

* One directory per tool name, each containing:

  * Tool metadata (runtime, main package, constraints).
  * `tool.lock` (similar to `px.lock` but tool-specific).
  * Tool env(s) tied to that lock.

Tools never modify project roots.

### 6.3 Commands

* `px tool install <name> [spec] [--python VERSION]`

  * Resolve and lock the specified package.

  * Bind to a chosen runtime:

    * Default: px’s default runtime.
    * Or explicit `--python`.

  * Materialize env for the tool.

* `px tool run <name> [-- …args]`

  * Look up tool by name.
  * Ensure the bound runtime is available and compatible.
  * Ensure env matches `tool.lock`.
  * Run the tool.

* `px tool list`

  * List installed tools, versions, and runtimes.

* `px tool remove <name>`

  * Remove tool metadata, lock, and env(s).

* `px tool upgrade <name>`

  * Re-resolve within specified constraints.
  * Update `tool.lock` and env.

### 6.4 Tool runtime selection & behavior on Python upgrades

Tool runtimes must follow a deterministic precedence:

1. **Install-time binding**

   * Each tool is installed against a specific runtime version:

     * From `--python VERSION` if provided, otherwise
     * From px’s default runtime at install time.

   * That runtime version is recorded in `tool.lock`.

2. **Run-time resolution**

   * `px tool run` must:

     * Look up the runtime recorded in `tool.lock`.
     * Use a px-managed interpreter for that exact version, if available.
     * If the interpreter is missing, fail with a clear PX error.

   * `px tool run` must **not** silently fall back to:

     * a different px-managed runtime (e.g. 3.11 → 3.12), or
     * the system Python.

3. **Python upgrades**

   If the runtime that a tool was locked against is no longer available or compatible:

   * `px tool run` must **fail clearly**, for example:

     ```text
     PX201  Tool 'black' was installed for Python 3.11, but only Python 3.12 is available.

     Why:
       • The original runtime for this tool is not installed.

     Fix:
       • Reinstall for the current runtime:  px tool install black
       • Or install Python 3.11 and rerun.
     ```

   * No silent breakage; no implicit re-resolution.

This mirrors the determinism guarantees for projects, but at the tool level.

---

## 7. Runtimes (`px python`)

### 7.1 Concept

A **runtime** is a Python interpreter that px can:

* Discover,
* Select for a project/tool,
* Record in config/lock.

### 7.2 Project runtime resolution

Order of precedence (deterministic):

1. `[tool.px].python` (explicit per-project setting, e.g. `"3.11"`).
2. `[project].requires-python` (PEP 621).
3. px default runtime.

If no available runtime satisfies constraints:

* Commands must fail with a clear explanation and suggest `px python install`.

px must **not** fall back to arbitrary system interpreters outside its runtime registry once a project is under px management.

### 7.3 Commands

* `px python list`

  * Show runtimes px knows, with version and path.

* `px python install <version>`

  * Downloads the requested CPython release via `python-build-standalone` (or similar; implementation detail), and
  * Installs under `~/.px/runtimes/…` and registers it in the px runtime registry.

* `px python use <version>`

  * For current project:

    * Record runtime choice in `[tool.px].python`.
    * Next `px sync` will rebuild env for that runtime.

* `px python info`

  * Show details about the active runtime for:

    * current project, and
    * default tool runtime.

---

## 8. Error, output & CI model

### 8.1 Error shape

All user-facing errors follow:

```text
PX123  <short summary>

Why:
  • <one or more bullet points>

Fix:
  • <one or more bullet points with copy-pasteable commands>
```

* Color:

  * Code + summary: error color.
  * “Why” bullets: normal.
  * “Fix” bullets: accent.

* Python tracebacks:

  * Shown *after* the px error summary by default.
  * Full raw trace only under `--debug`.

### 8.2 Common heuristics

Examples (non-exhaustive):

* **No project found**:

  * Suggest `px init`.

* **Lock missing / out-of-sync**:

  * Suggest `px sync` (or fail under `--frozen`).

* **Missing import in `px run`**:

  * Suggest `px add <pkg>` or `px sync` depending on whether `<pkg>` is already in M/L (§10.6.2).

* **Wrong interpreter (user ran `python` directly)**:

  * Suggest using `px run python ...`.

* **Runtime mismatch for tool**:

  * Suggest `px tool install <tool>` again or `px python install`.

### 8.3 Flags & CI behavior

* `-q / --quiet` – only essential output.
* `-v, -vv` – progressively more detail.
* `--debug` – full logs, internal details, stack traces.
* `--json` – structured output where applicable.

Under `CI=1` or explicit `--frozen`:

* No prompts.
* No auto-resolution.
* `px run` / `px test` / `px fmt` do **not** rebuild project/workspace envs; they just check consistency and fail if it’s broken (for `run`/`test`) or run tools in isolation (`fmt`).

### 8.4 Non-TTY & structured output (progress/logging)

To ensure deterministic behavior in logs and CI:

* If stderr is **not** a TTY, or `--json` is set:

  * Commands must not emit spinners, progress bars, or frame-based animations.
  * Progress must be represented as:

    * line-oriented log messages (for human mode), or
    * structured events inside `--json` output.

* In non-TTY mode:

  * Repeated progress updates should be throttled or collapsed; no flooding logs with frame-by-frame renders.
  * Output ordering must be stable for a given command and state.

This applies to all commands that show progress (e.g. resolver, env build, tool install).

---

## 9. Non-goals (updated)

px does **not**:

* Act as a general task runner (no `px task` DSL).
* Manage non-Python languages.
* Provide a plugin marketplace or unbounded extension API.
* Implicitly mutate state from read-only commands (`status`, `why`, `fmt` w.r.t. project/workspace state).
* Expose `cache` or `env` as primary user concepts.

Workspaces are an **advanced** concept for multi-project repos; most users can ignore them and treat px as a per-project tool.

If any future changes violate these, they’re design regressions, not “nice additions”.

---

## 10. Project state machine

### 10.1 Core entities

For each px project we define three artifacts:

* **M (Manifest)**
  Parsed `pyproject.toml`:

  * `[project].dependencies`
  * `[tool.px]` (including any px-specific dep config)
  * project-level Python constraints

* **L (Lock)**
  Parsed `px.lock`:

  * full resolved dependency graph
  * runtime (python version/tag)
  * platform tags
  * **manifest fingerprint** (see below)

* **E (Env)**
  The current project environment under `.px/envs/...`:

  * pointer to `L` it was built from (lock hash / ID)
  * actual installed packages (tracked by px’s metadata, not by re-scanning site-packages)
  * runtime used

You never introspect raw venv content to define state; you trust px’s own metadata.

Tool environments are separate and are **not** part of this state machine.

---

### 10.2 Identity & fingerprints

#### 10.2.1 Manifest fingerprint (`mfingerprint`)

From M, you compute a deterministic **manifest fingerprint**:

* Inputs:

  * `[project].dependencies`
  * any `[tool.px].dependencies` extensions/groups
  * relevant Python/version markers

* Output:

  * opaque hash, e.g. `sha256(hex-encoded)`.

Call this `mfingerprint(M)`.

#### 10.2.2 Lock identity

`px.lock` must store:

* `mfingerprint` it was computed from.
* A lock ID (e.g. hash of the full lock content): `l_id`.
* runtime & platform info.

So L is valid for exactly one `mfingerprint`.

#### 10.2.3 Env identity

Each environment E stores:

* `l_id` it was built from.
* runtime version & ABI.
* platform.

So we can answer “is E built from this L?” without scraping site-packages.

---

### 10.3 Derived state flags

For a given project, define these booleans:

* `manifest_exists` := `pyproject.toml` present and parseable with `[project]`.
* `lock_exists` := `px.lock` present and parseable.
* `env_exists` := px metadata shows at least one env for this project.

Assuming all three parse cleanly, define:

* `manifest_clean` := `lock_exists` and `L.mfingerprint == mfingerprint(M)` **and** `detect_lock_drift` reports no drift (version/mode/project/python mismatches are NeedsLock even when fingerprints match).
* `env_clean` := `env_exists` and `E.l_id == L.l_id`.

Then the **core invariant**:

```text
project_consistent := manifest_clean && env_clean
```

That’s the single boolean everything else should talk about.

---

### 10.4 Canonical project states

You can classify a project into a small set of states:

*Every* state report must carry the drift details that led to `NeedsLock` (e.g., `lock_issue: detect_lock_drift(...)`) so commands and diagnostics can surface the exact reason without recomputing.

#### 10.4.1 `Uninitialized`

* `manifest_exists == false`
* No lock, no env.

px commands:

* Only `px init` allowed here; everything else errors “no px project found”.

#### 10.4.2 `InitializedEmpty`

Fresh project with no deps:

* `manifest_exists == true`
* `[project].dependencies` is empty
* `lock_exists == true`, with empty graph and correct `mfingerprint`
* `env_exists == true`, `env_clean == true`
* So `project_consistent == true`

This is the state after a successful `px init`.

#### 10.4.3 `NeedsLock`

Manifest exists, no valid lock yet:

* `manifest_exists == true`
* (`lock_exists == false`) **or**
* (`lock_exists == true` but `manifest_clean == false`)
* `env_clean` is irrelevant here (env is defined *against* L).

Typical cause: user edited `pyproject.toml` manually or deleted `px.lock`.

#### 10.4.4 `NeedsEnv`

Manifest & lock agree, env out of date or missing:

* `manifest_clean == true`
* (`env_exists == false`) **or** (`env_clean == false`)

Typical cause: first install on a machine, or user wiped `.px/envs`.

#### 10.4.5 `Consistent`

Fully good state:

* `manifest_clean == true`
* `env_clean == true`
* i.e. `project_consistent == true`.

This is what you want after `init`, `add`, `remove`, `sync`, `update`, `migrate --apply`.

---

### 10.5 Command pre/post in terms of states

Now define commands *only* as transitions between these canonical states.

#### 10.5.1 `px init`

**Allowed start states**

* `Uninitialized`

**Behavior (success)**

* Create minimal M (manifest).
* Create empty L for chosen runtime (empty dep graph, correct `mfingerprint`).
* Create empty E matching L.

**End state**

* `InitializedEmpty` (which is also `Consistent`).

---

#### 10.5.2 `px add` / `px remove`

They’re both “mutable ops” over deps, then re-lock & rebuild:

**Allowed start states**

* Any state where `manifest_exists == true`:

  * `InitializedEmpty`, `Consistent`, `NeedsLock`, `NeedsEnv`.

**Required behavior**

* Modify M (add/remove deps).

* Resolve deps from new M, creating a new L:

  * `L'.mfingerprint == mfingerprint(M')`.

* Build E from L'.

**End state**

* Always `Consistent` (new `manifest_clean`, new `env_clean`).
* No matter what state you started from, a successful add/remove ends with `project_consistent == true`.

---

#### 10.5.3 `px sync [--frozen]`

**Purpose**
Reconcile M → L and then L → E.

**Allowed start states**

* Any state with `manifest_exists == true`.

**Behavior (dev)**

* If `lock_exists == false` or `manifest_clean == false`:

  * Resolve deps from M → new L.

* Ensure E built from current L (create/replace env if needed).

**End state**

* `Consistent`.

**Behavior (`--frozen` / CI)**

* If `lock_exists == false` or `manifest_clean == false`:

  * Fail. Do **not** resolve.

* Else:

  * Only fix env (if `env_clean == false`).

**End state (`--frozen`)**

* On success: `Consistent`.
* On failure: project state unchanged.

---

#### 10.5.4 `px update [<pkg>…]`

**Allowed start states**

* `manifest_exists == true` and `lock_exists == true`
  (in practice, you usually start from `Consistent`).

**Behavior**

* Take current M + L as input.
* Compute new L' with newer versions (bounded by constraints).
* Build E from L'.

**End state**

* `Consistent`.

**On resolution failure**

* No changes; state stays whatever it was before.

---

#### 10.5.5 `px run` / `px test`

Treat `px run` and `px test` as **readers** over M/L with **optional env repair** in dev mode – never M/L authors.

**Allowed start states (dev)**

* `Consistent` → run immediately.
* `NeedsEnv` → rebuild E from existing L, then run.

**Forbidden start states (dev)**

* `NeedsLock` (`manifest_clean == false`):

  * Do **not** create/update L.
  * Fail with:

    ```text
    PX120  Project manifest has changed since px.lock was created.

    Why:
      • pyproject.toml dependencies differ from px.lock.

    Fix:
      • Run `px sync` to update px.lock and the environment.
    ```

**Allowed start states (CI/`--frozen`)**

* Only `Consistent`. Anything else is a hard failure; no repair.

**Behavior (dev)**

* If `NeedsEnv`:

  * Rebuild E from L (no re-resolution).

* Run target via E.

**Behavior (CI/`--frozen`)**

* If not `Consistent`, fail.
* Never fix things; CI is a *check*, not a mutator.

**End state**

* M & L unchanged.
* E may be rehydrated → at end you’re either `Consistent` or unchanged.

---

#### 10.5.6 `px fmt` (project state)

`px fmt` intentionally **does not participate** in transitions of the project state machine beyond requiring a project root.

**Allowed start states**

* Any state where `manifest_exists == true`.

**Behavior**

* Does **not** read or modify L or E for decision-making.
* Does **not** resolve or rebuild E, in dev or CI.
* Runs only via tool envs (§6), which have their own isolated lifecycle.
* If `px.lock` or the project env is missing/drifted, `px fmt` still runs (or fails only on tool issues); state gates must not block it.

**End state**

* M, L, E unchanged.
* Only code files may be modified by formatter tools.

---

#### 10.5.7 `px status`

**Allowed start states**

* Any (including `Uninitialized`, in which case it reports that fact).

**Behavior**

* Compute and report:

  * `manifest_exists`, `lock_exists`, `env_exists`,
  * `manifest_clean`, `env_clean`,
  * derived canonical state.

**End state**

* No changes to M/L/E.

---

#### 10.5.8 `px migrate` / `px migrate --apply`

* `px migrate`:

  * Read-only; may start from non-px projects (effectively `Uninitialized`).
  * Produces a proposed M/L/E plan but does not write.

* `px migrate --apply`:

  * Creates M, L, and E from legacy inputs.
  * On success: ends in `Consistent`.
  * On failure: must not leave a partially migrated project.

---

### 10.6 Error & hint logic based on state

Now your missing-dep hints and drift messages can be defined purely in terms of this model.

#### 10.6.1 Manifest drift

You *only* ever say “Manifest drift detected” when:

* `manifest_exists == true`
* `lock_exists == true`
* `manifest_clean == false`

And **only** from commands that:

* Inspect state but don’t fix M/L (`run`, `test`, `fmt`, `status`).

Example behavior:

* `px run` sees `NeedsLock`:

  * Error:

    ```text
    PX120  Project manifest has changed since px.lock was created.

    Why:
      • pyproject.toml dependencies differ from px.lock.

    Fix:
      • Run `px sync` to update px.lock and the environment.
    ```

  * Do **not** attempt to fix L/E from `run`.

#### 10.6.2 Missing-import hint

On `ModuleNotFoundError: No module named 'pandas'` in `px run`:

1. Look at M:

   * If `pandas` is a direct dependency in M:

     * This is **env drift** (`env_clean == false` or broken E), not a missing add.
     * Suggest `px sync`, not `px add pandas`.

   * Else if `pandas` appears only as transitive in L:

     * Maybe suggest `px why pandas` or no hint.

   * Else (not in M or L):

     * Suggest `px add pandas`.

This aligns hinting with the actual state machine.

---

### 10.7 Why this is enough

With:

* Entities: M, L, E
* Fingerprints/IDs: `mfingerprint`, `l_id`
* Flags: `manifest_clean`, `env_clean`, `project_consistent`
* Canonical states: `Uninitialized`, `InitializedEmpty`, `NeedsLock`, `NeedsEnv`, `Consistent`
* Per-command “allowed writes” and allowed start/end states,

you can:

* enforce command contracts with simple checks,
* write tests like “starting from NeedsEnv, `px run` must end in Consistent or fail”,
* and make hinting logic deterministic instead of heuristic.

---

### 10.8 Command invariants (summary table, project commands)

This table summarizes the project-level command invariants in terms of the state machine. It is subordinate to the definitions above and does not introduce new semantics.

Legend:

* M = manifest (`pyproject.toml`)
* L = lock (`px.lock`)
* E = env (project env)
* States = { U = Uninitialized, IE = InitializedEmpty, NL = NeedsLock, NE = NeedsEnv, C = Consistent }

#### 10.8.1 Project lifecycle & deps

| Command              | Allowed start states | Writes M?                 | Writes L?                         | Writes E?                | Required end state (on success) | Notes                                                                     |
| -------------------- | -------------------- | ------------------------- | --------------------------------- | ------------------------ | ------------------------------- | ------------------------------------------------------------------------- |
| `px init`            | U                    | Yes                       | Yes                               | Yes                      | IE (also C)                     | Refuses if another tool clearly owns `pyproject.toml`.                    |
| `px add`             | IE, C, NL, NE        | Yes                       | Yes                               | Yes                      | C                               | Atomic: on resolver failure, no changes.                                  |
| `px remove`          | IE, C, NL, NE        | Yes                       | Yes                               | Yes                      | C                               | Only direct deps may be removed.                                          |
| `px sync`            | IE, C, NL, NE        | NL: L only; others: maybe | Yes (in dev or when lock missing) | Yes (if E dirty/missing) | C                               | Under `--frozen`/CI: never writes L; only repairs E if M/L already clean. |
| `px update`          | Any with M+L present | No                        | Yes                               | Yes                      | C                               | On resolution failure, no changes.                                        |
| `px migrate`         | Any (legacy or U)    | No                        | No                                | No                       | N/A                             | Read-only planning; prints proposal.                                      |
| `px migrate --apply` | Any (legacy or U)    | Yes                       | Yes                               | Yes                      | C                               | Must not leave partial migration on failure.                              |

#### 10.8.2 Execution & inspection

| Command     | Allowed start states (dev) | Allowed start states (CI/`--frozen`) | Writes M? | Writes L? | Writes E? (project) | Required end state (on success) | Notes                                                     |
| ----------- | -------------------------- | ------------------------------------ | --------- | --------- | ------------------- | ------------------------------- | --------------------------------------------------------- |
| `px run`    | C, NE                      | C                                    | No        | No        | Dev: NE→C; C→C      | C or unchanged                  | In dev, may repair E; in CI, never repairs E.             |
| `px test`   | C, NE                      | C                                    | No        | No        | Dev: NE→C; C→C      | C or unchanged                  | Same rules as `px run`.                                   |
| `px fmt`    | Any with `manifest_exists` | Any with `manifest_exists`           | No        | No        | No                  | Unchanged                       | Operates only on code and tool envs; never touches M/L/E. |
| `px status` | Any                        | Any                                  | No        | No        | No                  | Unchanged                       | Purely introspective.                                     |
| `px why`    | Any with `manifest_exists` | Any with `manifest_exists`           | No        | No        | No                  | Unchanged                       | Purely introspective.                                     |

This table is a testing and implementation aid: if an implementation observes behavior outside these invariants (e.g. `px fmt` rebuilding E, `px run` updating L, or `px tool run` switching runtimes), that behavior is a spec violation.

---

## 11. Workspace state machine (new)

### 11.1 Core entities

For each px workspace we define three artifacts:

* **WM (Workspace Manifest)**
  Derived from `[tool.px.workspace]` in the workspace root `pyproject.toml`:

  * list of member project paths (relative to workspace root),
  * optionally shared Python constraints, index config, etc.

  Each member project itself has a project manifest M in its own `pyproject.toml`.

* **WL (Workspace Lock)** – `px.workspace.lock`:

  * full resolved dependency graph across **all members**,
  * runtime (Python version/tag) for the workspace,
  * platform tags,
  * mapping from graph nodes to owning member project (or “external”),
  * **workspace manifest fingerprint** (see below).

* **WE (Workspace Env)**
  The workspace environment under `.px/...` at the workspace root:

  * pointer to WL it was built from (lock hash / ID),
  * actual installed packages (tracked by px’s metadata),
  * runtime used.

You never introspect raw venv content to define workspace state; you trust px’s metadata.

### 11.2 Identity & fingerprints

#### 11.2.1 Workspace manifest fingerprint (`wmfingerprint`)

From WM and the member manifests, compute a deterministic **workspace manifest fingerprint**:

* Inputs:

  * Workspace members list.
  * For each member:

    * `[project].dependencies` (and any `[tool.px].dependencies` extensions/groups),
    * relevant Python/version markers.

* Output:

  * opaque hash, e.g. `sha256(hex-encoded)`.

Call this `wmfingerprint(WM)`.

#### 11.2.2 Workspace lock identity

`px.workspace.lock` must store:

* `wmfingerprint` it was computed from.
* A workspace lock ID (e.g. hash of the full lock content): `wl_id`.
* runtime & platform info.

So WL is valid for exactly one `wmfingerprint`.

#### 11.2.3 Workspace env identity

Each workspace env WE stores:

* `wl_id` it was built from.
* runtime version & ABI.
* platform.

So we can answer “is WE built from this WL?” without scraping site-packages.

### 11.3 Derived workspace state flags

For a workspace, define:

* `w_manifest_exists` := `[tool.px.workspace]` present and parseable.
* `w_lock_exists` := `px.workspace.lock` present and parseable.
* `w_env_exists` := px metadata shows at least one workspace env for this root.

Assuming all three parse cleanly, define:

* `w_manifest_clean` := `w_lock_exists` and `WL.wmfingerprint == wmfingerprint(WM)`.
* `w_env_clean` := `w_env_exists` and `WE.wl_id == WL.wl_id`.

Core invariant:

```text
workspace_consistent := w_manifest_clean && w_env_clean
```

That’s the single boolean everything else should talk about at the workspace level.

### 11.4 Canonical workspace states

Analogous to projects:

* **`WUninitialized`**

  * `w_manifest_exists == false`
  * No workspace lock, no workspace env.

* **`WInitializedEmpty`**

  * `w_manifest_exists == true`
  * Member list may be empty or contain projects with empty deps.
  * `w_lock_exists == true` with correct `wmfingerprint` and empty/degenerate graph.
  * `w_env_exists == true`, `w_env_clean == true`.
  * So `workspace_consistent == true`.

* **`WNeedsLock`**

  * `w_manifest_exists == true` and
  * (`w_lock_exists == false` or `w_manifest_clean == false`).

  Typical cause: member manifests changed, or `px.workspace.lock` removed.

* **`WNeedsEnv`**

  * `w_manifest_clean == true` and
  * (`w_env_exists == false` or `w_env_clean == false`).

  Typical cause: first install on a machine, or user wiped workspace env.

* **`WConsistent`**

  * `w_manifest_clean == true`
  * `w_env_clean == true`.

This is what you want after workspace-level `sync`, `update`, or successful workspace-aware `add/remove` from members.

### 11.5 Workspace command behavior (using existing verbs)

Map existing commands to workspace states when invoked in workspace context:

* **`px sync` (from workspace root or member)**

  * Allowed start states: any with `w_manifest_exists == true`.
  * Dev behavior:

    * If `WNeedsLock`: resolve union of member manifests → new WL.
    * Ensure WE built from WL (`WConsistent`).
  * `--frozen` / CI:

    * If `WNeedsLock`: fail, no resolution.
    * If `WNeedsEnv`: rebuild WE only.
  * End state (success): `WConsistent`.

* **`px update` (from workspace root or member)**

  * Requires `w_manifest_exists` and `w_lock_exists`.
  * Updates WL (versions within constraints) and WE.
  * End state: `WConsistent`.

* **`px add` / `px remove` (from member)**

  * Modify only that member’s manifest.
  * Then same as “dev” `px sync` at workspace level:

    * recompute WL from all members,
    * rebuild WE.
  * End state: `WConsistent`.

* **`px run` / `px test` (from member)**

  * Allowed start states (dev): `WConsistent`, `WNeedsEnv`.
  * CI/`--frozen`: only `WConsistent`.
  * Dev may repair WE (no re-resolution); CI never repairs.

* **`px status`**

  * At workspace root:

    * report workspace state (`WUninitialized`, `WNeedsLock`, `WNeedsEnv`, `WConsistent`),
    * list members and whether their manifests were included in `wmfingerprint`.
  * In a member:

    * report workspace state plus a line like:

      * `member: included in workspace manifest: yes/no`.

### 11.6 Projects as workspace members

A project is a **workspace member** if:

* its path is listed (or matches a glob) in `[tool.px.workspace].members`, and
* the workspace root is an ancestor directory.

Rules:

* In workspace context:

  * The project’s manifest M is still authoritative for that project’s own metadata.
  * Its per-project lock/env (`px.lock`, project E) are **ignored** for resolution/materialization.
  * All dependency and env operations go through WL/WE.

* Outside workspace context (no applicable workspace root):

  * The project uses the **project** state machine (§10) as before, with `px.lock` and its own env.

This guarantees there is only one authority for deps/env at a time:

* either the **project** state machine (standalone), or
* the **workspace** state machine (for members).

---

## 12. Troubleshooting (error codes → required transitions)

* `missing_lock` (`PX120`): run `px sync` (without `--frozen`) to create or refresh `px.lock`.
* `lock_drift` (`PX120`): run `px sync` to realign `px.lock` with the manifest/runtime; frozen commands must refuse.
* `missing_env` / `env_outdated` (`PX201`): run `px sync` to (re)build the relevant project/workspace env; `--frozen` refuses to repair.
* `runtime_mismatch`: run `px sync` after activating the desired Python, or pin `tool.px.python`.
* `invalid_state`: delete or repair `.px/state.json` and retry; state is validated and rewritten atomically.
