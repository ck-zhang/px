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
Run configured formatters/linters/cleanup **tools** via their px-managed tool environments.

**Preconditions & behavior**

* Same env/lock consistency rules as `px run`.
* Honors `--frozen` / `CI=1` (no env rebuilds).
* If a required tool isn’t installed:

  * Suggest installing it (e.g. `px tool install ruff`).

**Postconditions**

* Code may be modified by the invoked tools.
* Manifest/lock unchanged; project consistency preserved.

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

  * Downloads the requested CPython release via `python-build-standalone` (fall back to `--path` for custom interpreters).
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
  * artifact/hashes
  * **manifest fingerprint** (see below)

* **E (Env)**
  The current project environment under `.px/envs/...`:

  * pointer to `L` it was built from (lock hash / ID)
  * actual installed packages (by px’s record, not by re-scanning site-packages)
  * runtime used

You never introspect raw venv content to define state; you trust px’s own metadata.

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

* `manifest_clean` := `lock_exists` and `L.mfingerprint == mfingerprint(M)`.
* `env_clean` := `env_exists` and `E.l_id == L.l_id`.

Then the **core invariant**:

```text
project_consistent := manifest_clean && env_clean
```

That’s the single boolean everything else should talk about.

---

### 10.4 Canonical project states

You can classify a project into a small set of states:

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

This is the state after a successful `px init` in your current design.

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

* `InitializedEmpty` (which is also `Consistent` in this model).

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

* `Consistent` (strictest) or more broadly: `manifest_exists == true` and `lock_exists == true`.

**Behavior**

* Take current M + L as input.
* Compute new L' with newer versions (bounded by constraints).
* Build E from L'.

**End state**

* `Consistent`.

**On resolution failure**

* No changes; state stays whatever it was before.

---

#### 10.5.5 `px run` / `px test` / `px fmt`

Treat these as **readers** with *optional env repair* – never M/L authors.

**Allowed start states (dev)**

* `Consistent` → run immediately.
* `NeedsEnv` → rebuild E from existing L, then run.

**Forbidden start states (dev)**

* `NeedsLock` (`manifest_clean == false`):

  * Do **not** create/update L.
  * Fail with:

    * “Manifest has changed; run `px sync` to update the lock.”

**Allowed start states (CI/`--frozen`)**

* Only `Consistent`. Anything else is a hard failure.

**Behavior (dev)**

* If `NeedsEnv`:

  * Rebuild E from L.
* Run target via E.

**Behavior (CI/`--frozen`)**

* If not `Consistent`, fail.
* Never fix things; CI is a *check*, not a mutator.

**End state**

* `M` & `L` unchanged.
* E may be rehydrated → at end you’re either `Consistent` or unchanged.

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

     * Maybe `px why pandas` or no hint.
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

No extra abstraction is needed; just wire all your commands and error paths to this model and stop letting them improvise.
