# px Spec (Authoritative)

## 0. What px is

px is the **front door for Python**:

* It owns **projects**, **tools**, and **runtimes**.
* It decides **which Python** runs, **which packages** are on `sys.path`, and **where they come from**.
* Once px is in charge, you should never have to ask:

  * “Which Python is this?”
  * “Why is this dependency even here?”
  * “Why did upgrading Python break my CLI tools?”

px is **not** a general task runner, multi-language build tool, or plugin marketplace.

---

## 1. Mental model

### 1.1 Nouns

px exposes three primary concepts:

* **Project**
  A directory with `pyproject.toml` and/or `px.lock` that px manages end-to-end.

* **Tool**
  A named Python CLI installed into its own isolated, CAS-backed environment, runnable from anywhere.

* **Runtime**
  A Python interpreter (e.g. 3.10, 3.11) that px knows about and can assign to projects and tools.

Everything else – envs, lockfiles, caches – are implementation details.

### 1.2 Project lifecycle (intended story)

For a typical user, Python “with px” looks like:

1. `px init` – declare this directory as a px project.
2. `px add ...` – declare dependencies.
3. `px sync` – resolve, lock, and build the environment.
4. `px run ...` / `px test` / `px fmt` – execute in the project’s env.
5. Commit `pyproject.toml` + `px.lock`.

That’s the core loop.

### 1.3 Tools lifecycle

For global tools:

1. `px tool install black`
2. `px tool run black --check .`

Tools are isolated from projects and from each other. Upgrading Python or changing project deps should not silently break them.

---

## 2. Filesystem & project shape

### 2.1 Project root & discovery

A directory is a **px project root** if it contains:

* `pyproject.toml` with `[tool.px]`, or
* `px.lock`.

For any project-level command, px:

1. Starts at CWD.
2. Walks upward until it finds a project root.
3. If none is found:
   `No px project found. Run "px init" in your project directory first.`

### 2.2 px-owned artifacts in a project

px may create/modify only:

* **User-facing / shared:**

  * `pyproject.toml`

    * px edits only `[project]` (PEP 621) and `[tool.px]` sections.
  * `dist/`

    * build artifacts (sdist, wheels).

* **px-specific:**

  * `px.lock` – locked dependency graph for this project.
  * `.px/` – all internal state:

    * `.px/envs/` – project envs (venv-like, but px-owned).
    * `.px/logs/` – logs.
    * `.px/state.json` – metadata (current env ID, lock hash, runtime, etc.).

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

px does **not** create `build/` etc. unless explicitly configured.

---

## 3. Global concepts

### 3.1 Lockfile

`px.lock` is the authoritative description of the project’s environment:

* Exact versions, hashes, markers, index URLs, platform tags.
* A fingerprint of the `[project].dependencies` (and any px-specific dep config).

It is **machine-generated only**; direct edits are unsupported.

### 3.2 Project environment

A **project environment** is a px-managed environment under `.px/envs/...`:

* It is tied to:

  * `px.lock` hash,
  * runtime (e.g. Python 3.11),
  * platform.
* It must contain exactly the packages described by `px.lock`.

### 3.3 Self-consistent project

A project is **self-consistent** if:

* `pyproject.toml` and `px.lock` agree (fingerprint matches).
* There exists a project environment whose identity matches `px.lock`.
* `px status` reports: `Environment in sync with px.lock`.

All mutating commands below (`init`, `add`, `remove`, `sync`, `update`, `migrate --apply`) must either:

* Leave the project self-consistent on success, or
* Fail without partial changes.

---

## 4. Command surface

### 4.1 Core project verbs

These are what most users should learn first:

* `px init`    – Create a new px project and empty lock/env.
* `px add`     – Add dependencies and update lock/env.
* `px remove`  – Remove dependencies and update lock/env.
* `px sync`    – Resolve (if needed) and sync env from lock.
* `px update`  – Upgrade dependencies within constraints and sync env.
* `px run`     – Run a command inside the project env.
* `px test`    – Run tests inside the project env.
* `px fmt`     – Run formatters/linters/cleanup for the project.
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

There is **no** `px cache`, `px env`, `px lock`, or `px workspace` top-level command.

---

## 5. Command contracts (authoritative semantics)

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
* Project is self-consistent.

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
* `px run` can immediately import the new packages.

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

  * Refuse with: `"<pkg> is not a direct dependency; px why <pkg> for more."`
* On resolution failure:

  * No change to `pyproject.toml`, `px.lock`, or env.

---

### 5.4 `px sync [--frozen]`

**Intent**
Make the project environment match declared state.

**Preconditions**

* Project root exists.

**Behavior**

1. **Lockfile phase**

   * Compute fingerprint of `[project].dependencies` (+ px dep config).
   * If `px.lock` is missing or fingerprint mismatch:

     * If `--frozen` or `CI=1`:

       * Fail: “px.lock missing or out of date; update locally and commit.”
     * Else:

       * Run resolver, write fresh `px.lock`.

2. **Environment phase**

   * If env is missing or its identity (hash/runtime) differs from `px.lock`:

     * Rebuild env from `px.lock`.

**Postconditions (success)**

* `px.lock` exists and matches `pyproject.toml`.
* Env matches `px.lock`.
* Project is self-consistent.

**Failure behavior**

* Env must not be left half-updated; operations should be transactional as far as possible.
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
Run a command using the project env.

**Preconditions**

* Project root exists.
* In non-CI/dev mode:

  * `px.lock` must exist and match `pyproject.toml`.
    If not, error and suggest `px sync`.
* In CI / `--frozen` mode:

  * Env must already be in sync; `px run` does not fix anything.

**Behavior (dev)**

* If env is missing or does not match `px.lock`:

  * Rebuild env from `px.lock` before running.
* Execute `<target>` inside the project env:

  * Could be:

    * a script file,
    * `python -m ...`,
    * a task alias from `[tool.px.scripts]` (if you define such a section).

**Behavior (strict / CI)**

* If `px.lock` missing or out-of-sync: fail.
* If env not in sync with `px.lock`: fail.

**Postconditions (success)**

* If env was rebuilt, project is self-consistent.
* Otherwise, project consistency unchanged.

**Failure behavior**

* If no px project found:
  `No px project found. Run "px init" in your project directory first.`
* If there is no `px.lock` or it’s out-of-date:
  Suggest `px sync`.
* If a `ModuleNotFoundError` points to a missing dep:

  * Add a px hint recommending `px add` for that module/package.

---

### 5.7 `px test`

**Intent**
Run tests in the project env, mirroring `px run`’s consistency rules.

**Preconditions & behavior**

* Same consistency semantics as `px run`:

  * In dev: may rebuild env from `px.lock` (no resolution).
  * In CI/`--frozen`: fails if lock/env out of sync.
* Discovers and runs the configured test runner (e.g. `pytest` by default).

**Postconditions**

* Same as `px run`.

---

### 5.8 `px fmt`

**Intent**
Run configured formatters/linters/cleanup tools in the project env.

**Preconditions & behavior**

* Same env/lock consistency semantics as `px run`.
* Accepts `--frozen` (or honors `CI=1`) to refuse env rebuilds and require a clean env.
* If required tools are missing:

  * px suggests adding them (e.g. `px add --group dev ruff`).

**Postconditions**

* Codebase may be modified by the tools.
* Project env/lock consistency maintained.

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

**Postconditions**

* None; read-only (except logs under `.px/logs/`).

---

### 5.10 `px migrate` / `px migrate --apply`

**Intent**
Convert a legacy Python project into a deterministic px project.

**`px migrate` (preview)**

* Reads legacy inputs:

  * e.g. `requirements.txt`, `Pipfile`, `poetry.lock`, existing venv.
* Computes a proposed:

  * `pyproject.toml`,
  * `px.lock`,
  * env plan.
* Prints a human-readable summary, **no writes**.

**`px migrate --apply`**

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

## 6. Tools

### 6.1 Concept

A **tool** is:

* A named entry point (e.g. `black`, `pytest`)…
* With its own **locked** env, CAS-backed, isolated from both:

  * project envs, and
  * other tools.

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

### 6.4 Behavior on Python upgrades

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

No silent breakage.

---

## 7. Runtimes (`px python`)

### 7.1 Concept

A **runtime** is a Python interpreter that px can:

* Discover,
* Select for a project/tool,
* Record in config/lock.

### 7.2 Project runtime resolution

Order of precedence:

1. `[tool.px].python` (explicit per-project setting, e.g. `"3.11"`).
2. `[project].requires-python` (PEP 621).
3. px default runtime.

If no available runtime satisfies constraints:

* Commands must fail with a clear explanation and suggest `px python install`.

### 7.3 Commands

* `px python list`

  * Show runtimes px knows, with version and path.

* `px python install <version>`

  * Install a runtime (implementation-specific), then add to px’s registry.

* `px python use <version>`

  * For current project:

    * Record runtime choice in `[tool.px].python`.
    * Next `px sync` will rebuild env for that runtime.

* `px python info`

  * Show details about the active runtime for:

    * current project, and
    * default tool runtime.

---

## 8. Error & output model

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

  * Suggest `px add <pkg>` or `px why <pkg>` if it’s supposed to be there.
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
* `px run` / `px test` / `px fmt` do **not** rebuild envs; they just check consistency and fail if it’s broken.

---

## 9. Non-goals (hard boundaries)

px does **not**:

* Act as a general task runner (no `px task` DSL).
* Manage non-Python languages.
* Provide a plugin marketplace or unbounded extension API.
* Implicitly mutate state from read-only commands (`status`, `why`).
* Expose `cache`, `env`, `lock`, or `workspace` as primary user concepts.

If any future changes violate these, they’re design regressions, not “nice additions”.
